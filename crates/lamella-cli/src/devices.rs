//! `lamella devices`: what is attached, and what to type to reach it.

use crate::args::{self, Spec};
use lamella_wire::{Capabilities, Negotiated};
use lamella_wire_host::{UsbTransport, hello_blocking, list_serial, open_target};
use std::fmt::Write as _;
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

/// What a row of the listing is, which decides the option that takes its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Something a program is sent to over Lamella Link: what `--device` takes.
    Device,
    /// A debug probe a board may be wired to: what `--probe` takes.
    Probe,
    /// A bootloader or programming drive an image is copied to: what `--volume` takes.
    Drive,
}

/// One attached thing, as the listing sees it.
struct Attached {
    /// The word the option for its kind takes.
    id: String,
    /// Which option that is.
    kind: Kind,
    /// What it is, in whatever terms the operating system reported.
    description: String,
}

/// What a device said when `--identify` asked it.
struct Answer {
    /// The board it answered as, when it answered.
    board: Option<String>,
    /// One word for how it answered, the status .NET lists a device with.
    status: &'static str,
    /// Everything it said, or why it said nothing.
    detail: String,
}

const USAGE: &str = "\
usage: lamella devices [--identify] [--all-devices]

Lists what is attached, as .NET lists the devices a program can run on: each one's ID, which is
the word --device takes, with its type, its status and what it is. Debug probes and bootloader
drives follow, each under the word --probe or --volume takes when a board is written over one.

A board in its bootloader is listed here too. It is still attached, and it is the one somebody
holding a new board is most likely to be looking for.

--identify asks each device what it is, over the wire, rather than reporting what the operating
system said about the port: the board it answers as becomes its description, and how it answered
its status. That costs a round trip per device and is the answer worth having when two boards look
alike.

--all-devices also lists the vendor-class USB devices this build decided are not debug probes. Use
it when a probe of yours is missing from the listing above: it is either not attached, or it is in
that list and this build did not recognize it.";

pub fn devices_command(args: &[String]) -> ExitCode {
    let spec = Spec {
        verb: "devices",
        usage: Some(USAGE),
        values: &[],
        flags: &["--identify", "--all-devices"],
    };
    let parsed = match args::parse_or_halt(args, &spec) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };

    let (mut attached, passed_over) = enumerate();
    for waiting in lamella_flash_routes::bootsel::waiting() {
        let mut description = waiting.state();
        if let Some(serial) = &waiting.serial {
            description.push_str(&format!("  serial {serial}"));
        }
        attached.push(Attached { id: waiting.volume.clone(), kind: Kind::Drive, description });
    }
    if attached.is_empty() {
        print!("{}", nothing_found());
        return ExitCode::SUCCESS;
    }

    let identifying = parsed.flag("--identify");
    let answers: Vec<Option<Answer>> = attached
        .iter()
        .map(|one| (identifying && one.kind == Kind::Device).then(|| ask(&one.id)))
        .collect();
    print!("{}", render(&attached, &answers));
    if !passed_over.is_empty() {
        let count = passed_over.len();
        println!(
            "{count} other vendor-class USB device(s) attached are not debug probes. A CMSIS-DAP\n\
             interface names itself as one and these did not, so they are something else -- but if\n\
             a probe of yours is missing above, it is among them."
        );
        if parsed.flag("--all-devices") {
            for device in &passed_over {
                println!("  {device}");
            }
        } else {
            println!("  Pass --all-devices to list them.");
        }
    }
    if attached.iter().any(|one| one.kind == Kind::Drive) {
        println!(
            "A DRIVE takes an image by having one COPIED to it, so it is no --device: nothing there\n\
             speaks a protocol. A bootloader's drive means the chip is halted waiting; an on-board\n\
             programmer's drive is there while that board runs."
        );
    }

    if identifying {
        println!("\nidentifying (a board answers only while Lamella firmware is running on it):");
        for (one, answer) in attached.iter().zip(&answers) {
            let Some(answer) = answer else { continue };
            println!("\n  {}", one.id);
            for line in answer.detail.lines() {
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
///
/// Also returns the vendor-class devices the probe filter declined, so the listing can account for
/// them rather than presenting a filtered bus as the whole one.
fn enumerate() -> (Vec<Attached>, Vec<lamella_probe::PassedOver>) {
    let mut attached = Vec::new();
    let mut passed_over = Vec::new();
    let probes = lamella_probe::list_reporting(&mut passed_over);
    for probe in &probes {
        let id = match &probe.serial {
            Some(serial) => serial.clone(),
            None => format!("? ({:04x}:{:04x}, no serial)", probe.vendor_id, probe.product_id),
        };
        let product = probe.product.clone().unwrap_or_else(|| "debug probe".to_owned());
        attached.push(Attached {
            id,
            kind: Kind::Probe,
            description: format!("{product}  a board may be wired to this over SWD"),
        });
    }
    if let Ok(boards) = UsbTransport::list() {
        for board in boards {
            let id = match &board.serial_number {
                Some(serial) => {
                    format!("usb:{:04x}:{:04x}:{serial}", board.vendor_id, board.product_id)
                }
                None => format!("usb:{:04x}:{:04x}", board.vendor_id, board.product_id),
            };
            let description =
                board.product.clone().unwrap_or_else(|| "(no product string)".to_owned());
            attached.push(Attached { id, kind: Kind::Device, description });
        }
    }
    let ports = list_serial();
    for port in &ports {
        let (id, note) = serial_target(port, &ports);
        let mut description = describe(port);
        if port.serial_number.as_deref().is_some_and(|serial| {
            probes.iter().any(|probe| probe.serial.as_deref() == Some(serial))
        }) {
            description.push_str("  [the UART bridge of the probe below, not a board]");
        }
        if let Some(note) = note {
            description.push_str(&format!("  {note}"));
        }
        attached.push(Attached { id, kind: Kind::Device, description });
    }
    (attached, passed_over)
}

/// The target to print for `port`, and a note when it is not the obvious one.
///
/// **A USB serial number is not unique per port.** One MCU-Link presents TWO virtual COM ports
/// under a single serial number, so `serial:<that number>` matches both -- and the resolver refuses
/// an ambiguous match rather than picking one, which is correct. A listing that printed that target
/// anyway would be handing the reader a string that cannot be opened, which is worse than printing
/// the unstable port name: the reader would blame the board.
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

/// HELLO the device `id` names, and say what came back, or why nothing did.
///
/// The id is opened through [`open_target`], which is the ONE place that knows the device
/// syntax. Reading an id here instead would put a second copy of that grammar behind the listing
/// that prints it, which is the one place it must not be.
fn ask(id: &str) -> Answer {
    let mut transport = match open_target(id, BAUD, IDENTIFY_TIMEOUT) {
        Ok(transport) => transport,
        Err(error) => {
            return Answer {
                board: None,
                status: "Cannot open",
                detail: format!("cannot open it: {error:?}"),
            };
        }
    };
    match hello_blocking(&mut transport, 1, host_caps(), IDENTIFY_TIMEOUT) {
        Ok(negotiated) => {
            let model = negotiated.identity.product_model;
            let board = lamella_wire::product_model::name(model)
                .map_or_else(|| format!("a board this build does not know (product model {model})"), str::to_owned);
            Answer { board: Some(board), status: "Online", detail: render_identity(&negotiated) }
        }
        Err(lamella_wire::TransportError::VersionMismatch { target_min, target_max }) => Answer {
            board: None,
            status: "Needs update",
            detail: lamella_wire_host::version_mismatch(
                lamella_wire::PROTOCOL_VERSION,
                target_min,
                target_max,
            ),
        },
        Err(_) => Answer {
            board: None,
            status: "No answer",
            detail: "no answer -- this is normal unless Lamella firmware is running on it".to_owned(),
        },
    }
}

/// The listing: the devices as .NET lists them -- an ID, a type, a status and a description, from
/// the `@(Devices)` item's identity and its `Type`, `Status` and `Description` -- and then the probes
/// and the drives, each under the word the option that takes it takes. `answers` is each row's
/// `--identify` answer, when there is one.
///
/// **THE FIRST COLUMN OF THE DEVICES IS WHAT `--device` TAKES**, and nothing else is in that
/// table: a probe and a drive are written through, never run on, so their words are `--probe`'s
/// and `--volume`'s and they are listed apart.
fn render(attached: &[Attached], answers: &[Option<Answer>]) -> String {
    let mut text = String::new();
    let rows = |kind: Kind| attached.iter().zip(answers).filter(move |(one, _)| one.kind == kind);
    let devices: Vec<Vec<String>> = rows(Kind::Device)
        .map(|(one, answer)| {
            let answer = answer.as_ref();
            vec![
                one.id.clone(),
                "Device".to_owned(),
                answer.map_or("-", |answer| answer.status).to_owned(),
                answer
                    .and_then(|answer| answer.board.clone())
                    .unwrap_or_else(|| one.description.clone()),
            ]
        })
        .collect();
    table(&mut text, &["ID", "TYPE", "STATUS", "DESCRIPTION"], &devices);
    for (kind, heading) in [(Kind::Probe, "PROBE"), (Kind::Drive, "DRIVE")] {
        let listed: Vec<Vec<String>> =
            rows(kind).map(|(one, _)| vec![one.id.clone(), one.description.clone()]).collect();
        if !text.is_empty() && !listed.is_empty() {
            text.push('\n');
        }
        table(&mut text, &[heading, "DESCRIPTION"], &listed);
    }
    let _ = write!(
        text,
        "\n{} attached. --device takes an ID; a board is written over a PROBE with --probe, or \
         through a DRIVE with --volume.\n",
        attached.len()
    );
    text
}

/// `rows` under `headings`, each column as wide as its widest cell, the last unpadded. Nothing at
/// all when there are no rows: an empty table says nothing a reader needs.
fn table(text: &mut String, headings: &[&str], rows: &[Vec<String>]) {
    if rows.is_empty() {
        return;
    }
    let widths: Vec<usize> = (0..headings.len())
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .chain([headings[column].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut line = |cells: Vec<&str>| {
        let last = cells.len() - 1;
        for (column, cell) in cells.into_iter().enumerate() {
            if column == last {
                text.push_str(cell);
            } else {
                let _ = write!(text, "{cell:<width$}  ", width = widths[column]);
            }
        }
        text.push('\n');
    };
    line(headings.to_vec());
    for row in rows {
        line(row.iter().map(String::as_str).collect());
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
            | Capabilities::DEBUG_BOOT_DEPLOYED,
    )
}

/// Render a negotiated identity: the board, its target ABI, which firmware build is answering, the
/// chip's own identity, and one line per resident runtime.
fn render_identity(negotiated: &Negotiated) -> String {
    let mut text = format!(
        "Lamella Link version {}, capabilities {:#010x}\n",
        negotiated.version, negotiated.caps.0
    );
    let identity = &negotiated.identity;
    let board = lamella_wire::product_model::name(identity.product_model)
        .unwrap_or("(a product_model this build does not recognize)");
    text.push_str(&format!("board {board} (product_model {})\n", identity.product_model));
    match lamella_wire::arch::name(identity.arch) {
        Some(name) => text.push_str(&format!("arch {name}\n")),
        None => text.push_str("arch: not reported by this firmware\n"),
    }
    if identity.firmware_version != [0, 0] {
        text.push_str(&format!(
            "firmware build day {} build {}\n",
            identity.firmware_version[0], identity.firmware_version[1]
        ));
    }
    if identity.chip_id_kind == lamella_wire::chip_id_kind::NONE {
        text.push_str("chip id: not reported by this firmware\n");
    } else {
        let hex: String = identity.chip_id.iter().map(|byte| format!("{byte:02x}")).collect();
        text.push_str(&format!("chip id (kind {}) {hex}\n", identity.chip_id_kind));
    }
    for surface in &identity.surfaces {
        let v = surface.lib_version;
        let f = surface.lib_file_version;
        text.push_str(&format!(
            "resident runtime: tier {} abi {} hash {:#018x} library {}.{}.{}.{} build {}.{}.{}.{}\n",
            surface.tier, surface.abi, surface.hash, v[0], v[1], v[2], v[3], f[0], f[1], f[2], f[3]
        ));
    }
    if let Some((tier, field)) = negotiated.unreadable_surface_version() {
        text.push_str(&format!(
            "WARNING: tier {tier} claims a resident library and reports no {field}. The firmware \
             could not read it, so this board cannot be version-compared against anything.\n"
        ));
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

    /// An MCU-Link presents two virtual COM ports under one USB serial number, so
    /// `serial:<that number>` resolves to two ports and the resolver refuses it. Printing it
    /// anyway would give the reader a target that cannot be opened -- and they would go looking at
    /// the board.
    #[test]
    fn a_serial_number_shared_by_two_ports_is_not_printed_as_a_target() {
        let all = vec![
            port("COM11", Some("PROBEALPHA001")),
            port("COM76", Some("PROBEALPHA001")),
            port("COM8", Some("SERIAL0000000001")),
            port("COM3", None),
        ];

        let (target, note) = serial_target(&all[0], &all);
        assert_eq!(target, "COM11", "the shared serial cannot address one port");
        assert!(note.expect("it explains itself").contains("2 ports"), "and says how many");

        let (target, note) = serial_target(&all[2], &all);
        assert_eq!(target, "serial:SERIAL0000000001", "a unique serial is the stable handle");
        assert!(note.is_none(), "and needs no explanation");

        let (target, note) = serial_target(&all[3], &all);
        assert_eq!(target, "COM3", "a port with no serial has only its name");
        assert!(note.is_none());
    }

    /// **THE DEVICES ARE LISTED AS .NET LISTS THEM, AND ONLY THEY ARE IN THAT TABLE.** Each device
    /// row opens with the word `--device` takes, then its type and status; an answer to
    /// `--identify` names the board and how it answered; probes and drives are listed apart, under
    /// the words their own options take.
    #[test]
    fn devices_are_listed_as_dotnet_lists_them_and_probes_and_drives_apart() {
        let row = |id: &str, kind, description: &str| Attached {
            id: id.to_owned(),
            kind,
            description: description.to_owned(),
        };
        let attached = [
            row("usb:39e9:0001:SERIAL0000000001", Kind::Device, "Lamella Link device"),
            row("COM8", Kind::Device, "COM8  USB Serial Device  (2e8a:000a)"),
            row("PROBE00001", Kind::Probe, "Debugprobe on Pico (CMSIS-DAP)  a board may be wired to this over SWD"),
            row("D:\\", Kind::Drive, "RP2350  HALTED in its bootloader, waiting for an image"),
        ];
        let answer = |board: Option<&str>, status| {
            Some(Answer { board: board.map(str::to_owned), status, detail: String::new() })
        };
        let answers = [answer(Some("Raspberry Pi Pico 2 W"), "Online"), answer(None, "No answer"), None, None];

        let text = render(&attached, &answers);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("ID ") && lines[0].ends_with("STATUS     DESCRIPTION"), "{text}");
        let type_column = lines[0].find("TYPE").expect("a TYPE column");
        assert!(lines[1].starts_with("usb:39e9:0001:SERIAL0000000001  Device  Online"), "{text}");
        assert!(lines[1].ends_with("Raspberry Pi Pico 2 W"), "the board it answered as: {text}");
        assert!(lines[2].starts_with("COM8 ") && lines[2].contains("Device  No answer"), "{text}");
        assert!(lines[2].ends_with("COM8  USB Serial Device  (2e8a:000a)"), "the OS's words, unanswered: {text}");
        for device in &lines[1..3] {
            assert_eq!(device.find("Device"), Some(type_column), "the columns line up: {text}");
        }
        assert_eq!(lines[3], "", "a probe is no device: {text}");
        assert!(lines[4].starts_with("PROBE ") && lines[5].starts_with("PROBE00001  Debugprobe"), "{text}");
        assert_eq!(lines[6], "", "{text}");
        assert!(lines[7].starts_with("DRIVE ") && lines[8].starts_with("D:\\ "), "{text}");
        assert!(text.contains("4 attached. --device takes an ID"), "{text}");
        assert!(text.lines().all(|line| line == line.trim_end()), "no padding after the last column: {text}");

        let unasked = render(&attached[..1], &[None]);
        assert!(unasked.lines().nth(1).is_some_and(|line| line.contains("Device  -       Lamella Link device")), "{unasked}");
        assert!(
            !unasked.lines().any(|line| line.starts_with("PROBE") || line.starts_with("DRIVE")),
            "an empty table is not printed: {unasked}"
        );
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
