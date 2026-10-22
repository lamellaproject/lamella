//! `lamella devices`: what is attached, and what to type to reach it.

use crate::args::{self, Spec};
use lamella_wire::{Capabilities, Negotiated};
use lamella_wire_host::{UsbTransport, hello_blocking};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// How long to wait for a HELLO_ACK under `--identify`.
///
/// Short on purpose: the device named may be any port, most ports are not running Lamella
/// firmware, and for those the whole answer is this wait.
const IDENTIFY_TIMEOUT: Duration = Duration::from_millis(1500);

/// The version of the two machine forms, `--json` and `--msbuild`.
///
/// It moves when a field is renamed, removed or given a new meaning. A field added beside the others
/// leaves it where it is, so a reader written for this version ignores what it does not know.
const SCHEMA: u32 = 1;

/// The `Type` every row carries: .NET's word for a physical device.
///
/// .NET leaves a type's values to whoever lists the devices. Its own phone workloads list `Device`
/// beside `Emulator` and `Simulator`, so a list that adds an emulator of its own sits beside these
/// rows without a clash.
const DEVICE_TYPE: &str = "Device";

/// The `Status` of a row nobody has asked: it is attached, which is all an unasked listing knows.
const ATTACHED: &str = "Connected";

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

impl Kind {
    /// The word the machine forms give this kind.
    fn word(self) -> &'static str {
        match self {
            Kind::Device => "device",
            Kind::Probe => "probe",
            Kind::Drive => "drive",
        }
    }
}

/// One attached thing, as the listing sees it.
struct Attached {
    /// The word the option for its kind takes.
    id: String,
    /// Which option that is.
    kind: Kind,
    /// What it is, in whatever terms the operating system reported.
    description: String,
    /// What a program reading the listing is told besides the words.
    facts: Facts,
}

/// The identifying details of a row, for the machine forms.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Facts {
    /// For a probe, its family: `cmsis-dap` or `st-link`.
    family: Option<&'static str>,
    /// For a CMSIS-DAP probe, the USB transport it speaks: `hid-v1` or `bulk-v2`.
    wire: Option<&'static str>,
    /// The USB vendor and product id.
    usb: Option<(u16, u16)>,
    /// The USB serial number the operating system reported.
    serial: Option<String>,
    /// The USB product string the operating system reported.
    product: Option<String>,
    /// For a serial port, the operating system's name for it.
    port: Option<String>,
    /// For a probe, the serial ports that share its USB serial number: its UART bridge or virtual
    /// COM port, which is where the console of a board wired to it can arrive.
    ports: Vec<String>,
}

/// A debug probe as the probe layer enumerated it.
struct ProbeSeen {
    /// The USB vendor id.
    vendor_id: u16,
    /// The USB product id.
    product_id: u16,
    /// The USB serial number, if the operating system reported one.
    serial: Option<String>,
    /// The USB product string, if the operating system reported one.
    product: Option<String>,
    /// The transport it is reached over, by its short name.
    wire: &'static str,
}

/// A native-USB Lamella Link board as the USB layer enumerated it.
struct BoardSeen {
    /// The USB vendor id.
    vendor_id: u16,
    /// The USB product id.
    product_id: u16,
    /// The USB serial number, if the operating system reported one.
    serial: Option<String>,
    /// The USB product string, if the operating system reported one.
    product: Option<String>,
}

/// What the device named to `--identify` said when it was asked.
struct Answer {
    /// The board it answered as, when it answered.
    board: Option<String>,
    /// One word for how it answered, the status .NET lists a device with: `Online`, `Offline`,
    /// `Busy` (it refused, and said why), `NeedsUpdate` or `CannotOpen`.
    status: &'static str,
    /// Everything it said, or why it said nothing.
    detail: String,
}

/// The form the listing is printed in.
#[derive(Debug, PartialEq, Eq)]
enum Form<'a> {
    /// The table a person reads.
    Table,
    /// JSON, on stdout.
    Json,
    /// An MSBuild project, written to this path.
    MsBuild(&'a str),
}

const USAGE: &str = "\
usage: lamella devices [--identify <device>] [--all-devices] [--json]

Lists what is attached, as .NET lists the devices a program can run on: each one's ID, which is
the word --device takes, with its type, its status and what it is. Debug probes and bootloader
drives follow, each under the word --probe or --volume takes when a board is written over one.

The listing opens nothing: every row is the USB identity the operating system reports, so a board
another program is using, or a probe in somebody's debug session, is listed undisturbed. A board
wired to a debug probe is listed as the probe's UART bridge, and which board that is stays unknown
until something asks it.

A board in its bootloader is listed here too. It is still attached, and it is the one somebody
holding a new board is most likely to be looking for.

--identify <device> asks the one device named what it is, over the wire: the board it answers as
becomes its description, and how it answered its status. It is opened as --device opens a device:
a serial port with DTR and RTS held low, except a Raspberry Pi Debug Probe's and an EDBG's, which
pass nothing back without them. On Linux and macOS the operating system raises both lines as it
opens the port, and a board that wires them to its reset restarts.

--all-devices also lists the vendor-class USB devices this build decided are not debug probes. Use
it when a probe of yours is missing from the listing above: it is either not attached, or it is in
that list and this build did not recognize it.

--json prints the same rows as JSON for a program to read: a schema version, then each row's id,
its kind (device, probe or drive: the option that takes the id), its type, status and description,
and, where it has them, its USB ids, serial number, probe family and serial ports. The vendor-class
devices passed over are listed under passed_over.";

/// The option that writes the listing as an MSBuild project, for a device SDK's
/// `ComputeAvailableDevices` target to read: the probes, each named by its USB serial number, with
/// the `Type`, `Status` and `Description` that `dotnet run --list-devices` shows. Written whole or not
/// at all, and nothing is printed unless writing it fails.
///
/// **NOT IN THE USAGE, AND ACCEPTED ONLY WHEN GIVEN.** It is internal to the SDK that reads the file
/// and the CLI it bundles, so the form can change with that SDK without taking back a public option.
const MSBUILD: &str = "--msbuild";

/// The option that names the one device to ask.
const IDENTIFY: &str = "--identify";

/// What `devices` accepts: the public options, and the internal one when it is given.
fn spec(args: &[String]) -> Spec<'static> {
    let internal = args.iter().any(|argument| argument == MSBUILD);
    Spec {
        verb: "devices",
        usage: Some(USAGE),
        values: if internal { &[IDENTIFY, MSBUILD] } else { &[IDENTIFY] },
        flags: &["--all-devices", "--json"],
    }
}

pub fn devices_command(args: &[String]) -> ExitCode {
    let parsed = match args::parse_or_halt(args, &spec(args)) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };
    let chosen = match form(&parsed) {
        Ok(chosen) => chosen,
        Err(refusal) => {
            eprintln!("{refusal}");
            return ExitCode::FAILURE;
        }
    };

    let (mut attached, passed_over) = enumerate();
    let drives = if matches!(chosen, Form::MsBuild(_)) { Vec::new() } else { lamella_flash_routes::bootsel::waiting() };
    for waiting in drives {
        let mut description = waiting.state();
        if let Some(serial) = &waiting.serial {
            description.push_str(&format!("  serial {serial}"));
        }
        attached.push(Attached {
            id: waiting.volume.clone(),
            kind: Kind::Drive,
            description,
            facts: Facts { serial: waiting.serial.clone(), ..Facts::default() },
        });
    }

    let named = parsed.value(IDENTIFY);
    let answer = named.map(ask);
    let answers = answered_rows(&attached, named, answer.as_ref());

    match chosen {
        Form::Json => {
            print!("{}", render_json(&attached, &answers, &passed_over));
            return ExitCode::SUCCESS;
        }
        Form::MsBuild(path) => {
            return match write_whole(Path::new(path), &render_msbuild(&attached)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("lamella: error: could not write the device listing to {path}: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        Form::Table => {}
    }

    if attached.is_empty() {
        print!("{}", nothing_found());
        return ExitCode::SUCCESS;
    }
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

    match (named, &answer) {
        (Some(named), Some(answer)) => {
            println!("\nidentifying {named} (a board answers only while Lamella firmware is running on it):");
            for line in answer.detail.lines() {
                println!("  {line}");
            }
        }
        _ => println!("`lamella devices --identify <ID>` asks one device what it is -- board, profile and chip id."),
    }
    ExitCode::SUCCESS
}

/// Each row's answer: the one `named` was given, for the row listed under that id, and none for the
/// rest. A device named that is not listed has no row, and its answer is printed after the table.
fn answered_rows<'a>(attached: &[Attached], named: Option<&str>, answer: Option<&'a Answer>) -> Vec<Option<&'a Answer>> {
    attached.iter().map(|one| answer.filter(|_| named == Some(one.id.as_str()))).collect()
}

/// The form `parsed` asks for, or why its options cannot go together.
///
/// An option that would change nothing in the form asked for is refused rather than ignored: a
/// listing that quietly dropped it would read as one that honored it.
fn form(parsed: &args::Options) -> Result<Form<'_>, String> {
    let json = parsed.flag("--json");
    let msbuild = parsed.value(MSBUILD);
    if json && msbuild.is_some() {
        return Err("lamella devices: --json prints the listing and --msbuild writes it to a file; \
                    give one of them."
            .to_owned());
    }
    if parsed.value(IDENTIFY).is_some_and(|device| device.starts_with("--")) {
        return Err("lamella devices: --identify takes the device to ask, as --device takes it: \
                    lamella devices --identify <ID>."
            .to_owned());
    }
    if msbuild.is_some() && parsed.value(IDENTIFY).is_some() {
        return Err("lamella devices: the MSBuild form is a listing, and a listing asks no device; \
                    leave --identify out."
            .to_owned());
    }
    if msbuild.is_some() && parsed.flag("--all-devices") {
        return Err("lamella devices: --all-devices widens the table, and the MSBuild form lists what \
                    an SDK can choose from; leave it out."
            .to_owned());
    }
    if json && parsed.flag("--all-devices") {
        return Err("lamella devices: --all-devices widens the table, and the JSON always lists the \
                    devices passed over; leave it out."
            .to_owned());
    }
    Ok(match msbuild {
        Some(path) => Form::MsBuild(path),
        None if json => Form::Json,
        None => Form::Table,
    })
}

/// Every attached board and probe, read from the USB identity the operating system holds: no port,
/// board or probe is opened, and nothing but a descriptor request reaches a device.
///
/// Also returns the vendor-class devices the probe filter declined, so the listing can account for
/// them rather than presenting a filtered bus as the whole one.
fn enumerate() -> (Vec<Attached>, Vec<lamella_probe::PassedOver>) {
    let mut passed_over = Vec::new();
    let probes = lamella_probe::list_reporting(&mut passed_over)
        .into_iter()
        .map(|probe| ProbeSeen {
            vendor_id: probe.vendor_id,
            product_id: probe.product_id,
            serial: probe.serial,
            product: probe.product,
            wire: probe.wire.tag(),
        })
        .collect();
    let boards = UsbTransport::list()
        .map(|boards| {
            boards
                .into_iter()
                .map(|board| BoardSeen {
                    vendor_id: board.vendor_id,
                    product_id: board.product_id,
                    serial: board.serial_number,
                    product: board.product,
                })
                .collect()
        })
        .unwrap_or_default();
    assemble(probes, passed_over, boards, &crate::ports::attached())
}

/// A debug probe as the listing finds it, with what a run needs to choose it and to reach a board
/// through it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProbeFound {
    /// Its USB serial number, which is what names it: `None` where the operating system reported
    /// none.
    pub(crate) serial: Option<String>,
    /// What it is, in the listing's words.
    pub(crate) description: String,
    /// Its USB vendor and product id.
    pub(crate) usb: (u16, u16),
    /// The serial ports that share its USB serial number: its UART bridge or virtual COM port.
    pub(crate) ports: Vec<String>,
}

/// The debug probes attached, found as `lamella devices` finds them.
pub(crate) fn attached_probes() -> Vec<ProbeFound> {
    let (attached, _passed_over) = enumerate();
    probes_among(attached)
}

/// The probes among the listing's rows.
fn probes_among(attached: Vec<Attached>) -> Vec<ProbeFound> {
    attached
        .into_iter()
        .filter(|one| one.kind == Kind::Probe)
        .map(|one| ProbeFound {
            serial: one.facts.serial,
            description: one.description,
            usb: one.facts.usb.unwrap_or_default(),
            ports: one.facts.ports,
        })
        .collect()
}

/// The listing's rows from what was enumerated: debug probes first, then native-USB Lamella Link
/// boards, then the operating system's serial ports.
///
/// Returns, beside the rows, the vendor-class devices that are still passed over once the ST-Links
/// among them have been recognized.
fn assemble(
    probes: Vec<ProbeSeen>,
    passed_over: Vec<lamella_probe::PassedOver>,
    boards: Vec<BoardSeen>,
    ports: &[lamella_wire_host::SerialPortDesc],
) -> (Vec<Attached>, Vec<lamella_probe::PassedOver>) {
    let mut attached = Vec::new();
    for probe in probes {
        let st_link = lamella_stlink::is_st_link(probe.vendor_id, probe.product_id);
        attached.push(Attached {
            id: probe_id(probe.serial.as_deref(), probe.vendor_id, probe.product_id),
            kind: Kind::Probe,
            description: probe.product.clone().unwrap_or_else(|| "debug probe".to_owned()),
            facts: Facts {
                family: Some(if st_link { "st-link" } else { "cmsis-dap" }),
                wire: (!st_link).then_some(probe.wire),
                usb: Some((probe.vendor_id, probe.product_id)),
                serial: probe.serial,
                product: probe.product,
                ..Facts::default()
            },
        });
    }
    let (st_links, passed_over): (Vec<_>, Vec<_>) = passed_over
        .into_iter()
        .partition(|device| lamella_stlink::is_st_link(device.vendor_id, device.product_id));
    for st_link in st_links {
        let description = lamella_stlink::product_id::name(st_link.product_id)
            .map(str::to_owned)
            .or_else(|| st_link.product.clone())
            .unwrap_or_else(|| "ST-Link".to_owned());
        attached.push(Attached {
            id: probe_id(st_link.serial.as_deref(), st_link.vendor_id, st_link.product_id),
            kind: Kind::Probe,
            description,
            facts: Facts {
                family: Some("st-link"),
                usb: Some((st_link.vendor_id, st_link.product_id)),
                serial: st_link.serial,
                product: st_link.product,
                ..Facts::default()
            },
        });
    }
    for board in boards {
        let id = match &board.serial {
            Some(serial) => format!("usb:{:04x}:{:04x}:{serial}", board.vendor_id, board.product_id),
            None => format!("usb:{:04x}:{:04x}", board.vendor_id, board.product_id),
        };
        let description = board.product.clone().unwrap_or_else(|| "(no product string)".to_owned());
        attached.push(Attached {
            id,
            kind: Kind::Device,
            description,
            facts: Facts {
                usb: Some((board.vendor_id, board.product_id)),
                serial: board.serial,
                product: board.product,
                ..Facts::default()
            },
        });
    }
    let probe_serials: Vec<String> = attached
        .iter()
        .filter(|one| one.kind == Kind::Probe)
        .filter_map(|one| one.facts.serial.clone())
        .collect();
    let carrying_probe = |port: &lamella_wire_host::SerialPortDesc| {
        let serial = port.serial_number.as_deref()?;
        probe_serials.iter().find(|probe| probe.eq_ignore_ascii_case(serial))
    };
    for port in ports {
        let (id, note) = serial_target(port, ports);
        let mut description = describe(port);
        if let Some(probe) = carrying_probe(port) {
            description.push_str(&format!("  [the UART bridge of probe {probe}: target not identified]"));
        }
        if let Some(note) = note {
            description.push_str(&format!("  {note}"));
        }
        attached.push(Attached {
            id,
            kind: Kind::Device,
            description,
            facts: Facts {
                usb: port.vid.zip(port.pid),
                serial: port.serial_number.clone(),
                product: port.product.clone(),
                port: Some(port.port.clone()),
                ..Facts::default()
            },
        });
    }
    for probe in attached.iter_mut().filter(|one| one.kind == Kind::Probe) {
        let Some(serial) = &probe.facts.serial else { continue };
        probe.facts.ports = ports
            .iter()
            .filter(|port| {
                port.serial_number.as_deref().is_some_and(|other| other.eq_ignore_ascii_case(serial))
            })
            .map(|port| port.port.clone())
            .collect();
    }
    (attached, passed_over)
}

/// The id a probe is listed under: its USB serial number, which `--probe` and
/// `LAMELLA_PROBE_SERIAL` take. A probe the operating system reports no serial number for cannot be
/// named, and its id says so.
fn probe_id(serial: Option<&str>, vendor_id: u16, product_id: u16) -> String {
    match serial {
        Some(serial) => serial.to_owned(),
        None => format!("? ({vendor_id:04x}:{product_id:04x}, no serial)"),
    }
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

/// Whether `usb` is the USB identity of Lamella firmware, a board that names itself as running it.
/// The current pair only: the one it used before is a shared hobby id, which names nothing alone.
fn names_lamella_firmware(usb: Option<(u16, u16)>) -> bool {
    usb == Some((lamella_wire::usb::VID, lamella_wire::usb::PID))
}

/// Whether `usb` is the identity a debug probe's UART bridge enumerates under, the way a board
/// running Lamella firmware is reached through the probe it is wired to: a Raspberry Pi Debug
/// Probe's, a DAPLink's, an MCU-Link's, an EDBG's or an ST-LINK's.
///
/// **IT DECIDES WHAT THE SDK LISTS, NOT WHAT IS OPENED.** A listing opens nothing, and the device a
/// user names is opened whatever it is ([`crate::deploy::open_named`]).
fn is_probe_bridge(usb: Option<(u16, u16)>) -> bool {
    let Some((vendor, product)) = usb else {
        return false;
    };
    matches!((vendor, product), (0x2e8a, 0x000c) | (0x0d28, 0x0204) | (0x1fc9, 0x0143) | (0x03eb, 0x2111))
        || lamella_stlink::is_st_link(vendor, product)
}

/// Whether the bridge with the USB identity `usb` is opened with DTR and RTS asserted: a Raspberry
/// Pi Debug Probe's and an EDBG's, the two that pass the board's bytes to the computer only while DTR
/// is asserted. The Debug Probe's UART connector carries no reset line, and an EDBG holds its UART
/// pins tristated until DTR is asserted.
pub(crate) fn raises_lines(usb: Option<(u16, u16)>) -> bool {
    usb == Some((0x2e8a, 0x000c)) || usb == Some((0x03eb, 0x2111))
}

/// HELLO the device `target` names, and say what came back, or why nothing did.
///
/// It is opened as `--device` opens a device, by [`crate::deploy::open_named`], the one place that
/// knows how: a second copy of that rule here would be the one a listing's caller trusts.
fn ask(target: &str) -> Answer {
    let mut transport = match crate::deploy::open_named(target, IDENTIFY_TIMEOUT) {
        Ok(transport) => transport,
        Err(why) => return Answer { board: None, status: "CannotOpen", detail: why },
    };
    answer_of(hello_blocking(&mut transport, 1, host_caps(), IDENTIFY_TIMEOUT))
}

/// What the outcome of a HELLO says about the device that was asked.
fn answer_of(hello: Result<Negotiated, lamella_wire::TransportError>) -> Answer {
    match hello {
        Ok(negotiated) => {
            let model = negotiated.identity.product_model;
            let board = lamella_wire::product_model::name(model)
                .map_or_else(|| format!("a board this build does not know (product model {model})"), str::to_owned);
            Answer { board: Some(board), status: "Online", detail: render_identity(&negotiated) }
        }
        Err(lamella_wire::TransportError::VersionMismatch { target_min, target_max }) => Answer {
            board: None,
            status: "NeedsUpdate",
            detail: lamella_wire_host::version_mismatch(lamella_wire::PROTOCOL_VERSION, target_min, target_max),
        },
        Err(lamella_wire::TransportError::Refused { reason, .. }) => Answer {
            board: None,
            status: "Busy",
            detail: format!(
                "it answered and refused: {}",
                lamella_wire_host::on_device::describe_refusal(reason)
            ),
        },
        Err(error) => Answer {
            board: None,
            status: "Offline",
            detail: format!("no answer ({error:?}) -- this is normal unless Lamella firmware is running on it"),
        },
    }
}

/// A row's status and description, as every form shows them: an `--identify` answer gives the
/// status it answered with and, when it named one, the board as the description.
fn presented<'a>(one: &'a Attached, answer: Option<&'a Answer>) -> (&'a str, &'a str) {
    let status = answer.map_or(ATTACHED, |answer| answer.status);
    let description =
        answer.and_then(|answer| answer.board.as_deref()).unwrap_or(one.description.as_str());
    (status, description)
}

/// The listing: the devices as .NET lists them -- an ID, a type, a status and a description, from
/// the `@(Devices)` item's identity and its `Type`, `Status` and `Description` -- and then the probes
/// and the drives, each under the word the option that takes it takes. `answers` holds, on the row of
/// the device `--identify` named, its answer.
///
/// **THE FIRST COLUMN OF THE DEVICES IS WHAT `--device` TAKES**, and nothing else is in that
/// table: a probe and a drive are written through, never run on, so their words are `--probe`'s
/// and `--volume`'s and they are listed apart.
fn render(attached: &[Attached], answers: &[Option<&Answer>]) -> String {
    let mut text = String::new();
    let rows = |kind: Kind| attached.iter().zip(answers).filter(move |(one, _)| one.kind == kind);
    let devices: Vec<Vec<String>> = rows(Kind::Device)
        .map(|(one, answer)| {
            let (status, description) = presented(one, *answer);
            vec![one.id.clone(), DEVICE_TYPE.to_owned(), status.to_owned(), description.to_owned()]
        })
        .collect();
    table(&mut text, &["ID", "TYPE", "STATUS", "DESCRIPTION"], &devices);
    for (kind, heading) in [(Kind::Probe, "PROBE"), (Kind::Drive, "DRIVE")] {
        let listed: Vec<Vec<String>> = rows(kind)
            .map(|(one, _)| {
                let description = if kind == Kind::Probe {
                    format!("{}  a board may be wired to this over SWD", one.description)
                } else {
                    one.description.clone()
                };
                vec![one.id.clone(), description]
            })
            .collect();
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

/// The listing as JSON: the schema version, every row with its kind, type, status, description and
/// identifying details, and the vendor-class devices passed over.
fn render_json(
    attached: &[Attached],
    answers: &[Option<&Answer>],
    passed_over: &[lamella_probe::PassedOver],
) -> String {
    let hex = |value: u16| format!("{value:04x}");
    let rows: Vec<Value> = attached
        .iter()
        .zip(answers)
        .map(|(one, answer)| {
            let (status, description) = presented(one, *answer);
            json!({
                "id": one.id,
                "kind": one.kind.word(),
                "type": DEVICE_TYPE,
                "status": status,
                "description": description,
                "family": one.facts.family,
                "wire": one.facts.wire,
                "vid": one.facts.usb.map(|(vendor_id, _)| hex(vendor_id)),
                "pid": one.facts.usb.map(|(_, product_id)| hex(product_id)),
                "serial": one.facts.serial,
                "product": one.facts.product,
                "port": one.facts.port,
                "ports": one.facts.ports,
            })
        })
        .collect();
    let passed: Vec<Value> = passed_over
        .iter()
        .map(|device| {
            json!({
                "vid": hex(device.vendor_id),
                "pid": hex(device.product_id),
                "serial": device.serial,
                "product": device.product,
                "interface": device.interface_name,
            })
        })
        .collect();
    let document = json!({ "schema": SCHEMA, "devices": rows, "passed_over": passed });
    let mut text = serde_json::to_string_pretty(&document).unwrap_or_default();
    text.push('\n');
    text
}

/// The listing as an MSBuild project with two targets, for an SDK's `ComputeAvailableDevices` to read
/// through the `MSBuild` task's `TargetOutputs`. Every item carries `Type`, `Status` and
/// `Description`, the metadata `dotnet run --list-devices` reads, and never `RuntimeIdentifier`,
/// which `dotnet run` would apply to the whole build.
///
/// `LamellaProbes` returns one item per probe, named by its USB serial number, with `Kind`, `Family`
/// and `Usb`; a probe with no serial number is left out, because nothing could name it.
///
/// `LamellaDevices` returns the devices a program can be sent to, each named by the id `--device`
/// takes: a board whose USB identity is Lamella firmware's own, and a debug probe's UART bridge, which
/// carries `Bridge="true"` because the board behind it is not identified. A port of any other
/// identity is left out, as is everything a listing cannot name without opening it.
///
/// Every value is escaped by [`attribute`], so no device string can become a property, an item
/// list, a wildcard or a separator in the reader's build.
fn render_msbuild(attached: &[Attached]) -> String {
    let mut text = String::from(
        "<Project>\n  <!-- Written by the lamella devices command: what is attached to this computer. -->\n",
    );
    let _ = writeln!(
        text,
        "  <PropertyGroup>\n    <LamellaDevicesSchema>{SCHEMA}</LamellaDevicesSchema>\n  </PropertyGroup>"
    );
    text.push_str("  <Target Name=\"LamellaProbes\" Returns=\"@(LamellaProbe)\">\n    <ItemGroup>\n");
    for one in attached.iter().filter(|one| one.kind == Kind::Probe && one.facts.serial.is_some()) {
        let (status, description) = presented(one, None);
        let _ = write!(
            text,
            "      <LamellaProbe Include=\"{}\" Type=\"{}\" Status=\"{}\" Description=\"{}\" Kind=\"{}\"",
            attribute(&one.id),
            attribute(DEVICE_TYPE),
            attribute(status),
            attribute(description),
            attribute(one.kind.word()),
        );
        if let Some(family) = one.facts.family {
            let _ = write!(text, " Family=\"{}\"", attribute(family));
        }
        if let Some((vendor_id, product_id)) = one.facts.usb {
            let _ = write!(text, " Usb=\"{vendor_id:04x}:{product_id:04x}\"");
        }
        text.push_str(" />\n");
    }
    text.push_str("    </ItemGroup>\n  </Target>\n");
    text.push_str("  <Target Name=\"LamellaDevices\" Returns=\"@(LamellaDevice)\">\n    <ItemGroup>\n");
    for one in attached.iter().filter(|one| one.kind == Kind::Device) {
        let bridge = is_probe_bridge(one.facts.usb);
        if !bridge && !names_lamella_firmware(one.facts.usb) {
            continue;
        }
        let (status, description) = presented(one, None);
        let _ = write!(
            text,
            "      <LamellaDevice Include=\"{}\" Type=\"{}\" Status=\"{}\" Description=\"{}\"",
            attribute(&one.id),
            attribute(DEVICE_TYPE),
            attribute(status),
            attribute(description),
        );
        if bridge {
            text.push_str(" Bridge=\"true\"");
        }
        text.push_str(" />\n");
    }
    text.push_str("    </ItemGroup>\n  </Target>\n</Project>\n");
    text
}

/// `text` as the value of one MSBuild attribute: each character MSBuild treats specially as its
/// `%XX` escape, then each character XML treats specially as a reference.
///
/// MSBuild's are `%`, `$`, `@`, `'`, `;`, `?` and `*`; left as they are, they would read as an item's
/// metadata, a property, an item list, part of a condition, a list separator or a wildcard. A
/// control character, which no device string should hold and an XML attribute cannot carry, becomes
/// a space.
pub(crate) fn attribute(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '%' | '$' | '@' | '\'' | ';' | '?' | '*' => {
                let _ = write!(out, "%{:02X}", u32::from(ch));
            }
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\u{fffe}' | '\u{ffff}' => out.push(' '),
            ch if ch.is_control() => out.push(' '),
            ch => out.push(ch),
        }
    }
    out
}

/// Writes `text` to `path` whole or not at all: into a file beside it, then renamed over it, so a
/// reader never finds half a listing.
fn write_whole(path: &Path, text: &str) -> std::io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".{}.tmp", std::process::id()));
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
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

    /// A row with no identifying details, as the table tests need.
    fn row(id: &str, kind: Kind, description: &str) -> Attached {
        Attached { id: id.to_owned(), kind, description: description.to_owned(), facts: Facts::default() }
    }

    /// **THE SDK IS OFFERED LAMELLA's OWN IDENTITY AND THE PROBES' UART BRIDGES.** A generic USB-UART
    /// chip, a board's own bridge and a port with no USB identity are not offered; the bridges of a
    /// Debug Probe and an EDBG are the two opened with DTR and RTS up.
    #[test]
    fn the_sdk_is_offered_lamella_firmware_and_the_probe_bridges() {
        assert!(names_lamella_firmware(Some((lamella_wire::usb::VID, lamella_wire::usb::PID))));
        assert!(!names_lamella_firmware(Some((lamella_wire::usb::LEGACY_VID, lamella_wire::usb::PID))), "a shared hobby id");
        for (usb, what) in [
            ((0x2e8a, 0x000c), "a Raspberry Pi Debug Probe"),
            ((0x0d28, 0x0204), "a DAPLink"),
            ((0x1fc9, 0x0143), "an MCU-Link"),
            ((0x0483, 0x374b), "an ST-LINK/V2-1"),
            ((0x0483, 0x374e), "an STLINK-V3"),
            ((0x03eb, 0x2111), "an EDBG"),
        ] {
            assert!(is_probe_bridge(Some(usb)), "{what} is a probe's bridge");
        }
        for (usb, what) in [
            ((0x0403, 0x6001), "an FTDI bridge"),
            ((0x1a86, 0x7523), "a CH340"),
            ((0x10c4, 0xea60), "a CP210x"),
            ((0x2341, 0x0043), "an Arduino Uno's 16U2"),
            ((0x303a, 0x1001), "an Espressif USB-Serial-JTAG"),
            ((0x2e8a, 0x000a), "an RP2040 running other firmware"),
        ] {
            assert!(!is_probe_bridge(Some(usb)) && !names_lamella_firmware(Some(usb)), "{what} is not offered");
        }
        assert!(!is_probe_bridge(None) && !names_lamella_firmware(None), "a port with no USB identity is not offered");
        assert!(raises_lines(Some((0x2e8a, 0x000c))), "a Debug Probe's bridge is opened with DTR and RTS up");
        assert!(raises_lines(Some((0x03eb, 0x2111))), "and an EDBG's");
        for usb in [(0x0d28, 0x0204), (0x1fc9, 0x0143), (0x0483, 0x374b), (0x0403, 0x6001), (lamella_wire::usb::VID, lamella_wire::usb::PID)] {
            assert!(!raises_lines(Some(usb)), "{usb:04x?} is opened with both lines down");
        }
    }

    /// **ONLY THE DEVICE NAMED IS ANSWERED, ON THE ROW LISTED UNDER ITS ID**; every other row keeps
    /// what the operating system reported, and a name no row carries answers on none of them.
    #[test]
    fn only_the_device_named_is_answered() {
        let attached = [
            row("serial:E6640000000000A1", Kind::Device, "COM75  Debugprobe on Pico (CMSIS-DAP)  (2e8a:000c)"),
            row("COM5", Kind::Device, "COM5  USB Serial Port  (0403:6001)"),
        ];
        let answer = Answer { board: Some("Raspberry Pi Pico 2 W".to_owned()), status: "Online", detail: String::new() };
        let answers = answered_rows(&attached, Some("COM5"), Some(&answer));
        assert!(answers[0].is_none() && answers[1].is_some(), "the named row alone");
        assert!(answered_rows(&attached, Some("COM9"), Some(&answer)).iter().all(Option::is_none));
        assert!(answered_rows(&attached, None, None).iter().all(Option::is_none), "a listing answers nothing");
    }

    /// A CMSIS-DAP v2 probe as the probe layer reports one.
    fn cmsis_dap(serial: Option<&str>) -> ProbeSeen {
        ProbeSeen {
            vendor_id: 0x2e8a,
            product_id: 0x000c,
            serial: serial.map(str::to_owned),
            product: Some("Debugprobe on Pico (CMSIS-DAP)".to_owned()),
            wire: "bulk-v2",
        }
    }

    /// A vendor-class device the CMSIS-DAP filter declined.
    fn declined(vendor_id: u16, product_id: u16, serial: &str, product: &str) -> lamella_probe::PassedOver {
        lamella_probe::PassedOver {
            vendor_id,
            product_id,
            serial: Some(serial.to_owned()),
            product: Some(product.to_owned()),
            interface_name: Some("ST-Link Debug".to_owned()),
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

    /// **A REFUSAL READS AS A REFUSAL, NAMING THE REASON THE BOARD GAVE**, and silence as silence,
    /// with what the carrier said. A board that refuses because another carrier holds its session is
    /// running Lamella firmware, which the sentence for silence says it is not.
    #[test]
    fn an_identify_refusal_is_an_answer_and_says_why() {
        let refused = answer_of(Err(lamella_wire::TransportError::Refused {
            reason: lamella_wire::error::SESSION_HELD,
            msg_type: lamella_wire::msg::HELLO,
            holder: Some(2),
        }));
        assert_eq!((refused.status, refused.board.as_deref()), ("Busy", None));
        assert!(
            refused.detail.starts_with("it answered and refused: another carrier already holds the debug session"),
            "{}",
            refused.detail
        );

        let silent = answer_of(Err(lamella_wire::TransportError::Closed));
        assert_eq!(silent.status, "Offline");
        assert_eq!(silent.detail, "no answer (Closed) -- this is normal unless Lamella firmware is running on it");
    }

    /// **THE DEVICES ARE LISTED AS .NET LISTS THEM, AND ONLY THEY ARE IN THAT TABLE.** Each device
    /// row opens with the word `--device` takes, then its type and status; an answer to
    /// `--identify` names the board and how it answered; probes and drives are listed apart, under
    /// the words their own options take.
    #[test]
    fn devices_are_listed_as_dotnet_lists_them_and_probes_and_drives_apart() {
        let attached = [
            row("usb:39e9:0001:SERIAL0000000001", Kind::Device, "Lamella Link device"),
            row("COM8", Kind::Device, "COM8  USB Serial Device  (2e8a:000a)"),
            row("PROBE00001", Kind::Probe, "Debugprobe on Pico (CMSIS-DAP)"),
            row("D:\\", Kind::Drive, "RP2350  HALTED in its bootloader, waiting for an image"),
        ];
        let online = Answer { board: Some("Raspberry Pi Pico 2 W".to_owned()), status: "Online", detail: String::new() };
        let offline = Answer { board: None, status: "Offline", detail: String::new() };
        let answers = [Some(&online), Some(&offline), None, None];

        let text = render(&attached, &answers);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("ID ") && lines[0].ends_with("STATUS   DESCRIPTION"), "{text}");
        let type_column = lines[0].find("TYPE").expect("a TYPE column");
        assert!(lines[1].starts_with("usb:39e9:0001:SERIAL0000000001  Device  Online"), "{text}");
        assert!(lines[1].ends_with("Raspberry Pi Pico 2 W"), "the board it answered as: {text}");
        assert!(lines[2].starts_with("COM8 ") && lines[2].contains("Device  Offline"), "{text}");
        assert!(lines[2].ends_with("COM8  USB Serial Device  (2e8a:000a)"), "the OS's words, unanswered: {text}");
        for device in &lines[1..3] {
            assert_eq!(device.find("Device"), Some(type_column), "the columns line up: {text}");
        }
        assert_eq!(lines[3], "", "a probe is no device: {text}");
        assert!(lines[4].starts_with("PROBE ") && lines[5].starts_with("PROBE00001  Debugprobe"), "{text}");
        assert!(lines[5].ends_with("(CMSIS-DAP)  a board may be wired to this over SWD"), "{text}");
        assert_eq!(lines[6], "", "{text}");
        assert!(lines[7].starts_with("DRIVE ") && lines[8].starts_with("D:\\ "), "{text}");
        assert!(text.contains("4 attached. --device takes an ID"), "{text}");
        assert!(text.lines().all(|line| line == line.trim_end()), "no padding after the last column: {text}");

        let unasked = render(&attached[..1], &[None]);
        assert!(unasked.lines().nth(1).is_some_and(|line| line.contains("Device  Connected  Lamella Link device")), "{unasked}");
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

    /// **AN ST-LINK IS LISTED AS THE PROBE IT IS.** The CMSIS-DAP filter declines it, and its
    /// vendor and product id bring it back: named by its serial number, its family `st-link`, its
    /// description the generation the id names -- and it no longer counts among the devices that
    /// are not probes, while a device that really is not one still does.
    #[test]
    fn an_st_link_the_cmsis_dap_filter_passed_over_is_listed_as_a_probe() {
        let passed_over = vec![
            declined(0x0483, 0x374b, "066DFF555051897267143327", "STM32 STLink"),
            declined(0x1915, 0x520f, "NOTAPROBE01", "a radio dongle"),
        ];
        let (attached, still_passed_over) = assemble(Vec::new(), passed_over, Vec::new(), &[]);

        assert_eq!(attached.len(), 1, "the ST-Link alone becomes a row");
        let st_link = &attached[0];
        assert_eq!(st_link.id, "066DFF555051897267143327");
        assert_eq!(st_link.kind, Kind::Probe);
        assert_eq!(st_link.description, "ST-LINK/V2-1");
        assert_eq!(st_link.facts.family, Some("st-link"));
        assert_eq!(st_link.facts.wire, None, "an ST-Link speaks no CMSIS-DAP wire");
        assert_eq!(st_link.facts.usb, Some((0x0483, 0x374b)));
        assert_eq!(st_link.facts.product.as_deref(), Some("STM32 STLink"));
        assert_eq!(still_passed_over.len(), 1, "{still_passed_over:?}");
        assert_eq!(still_passed_over[0].vendor_id, 0x1915, "the device that is not a probe stays counted");
    }

    /// A probe the CMSIS-DAP filter kept because its interface name was unknown is still told by its
    /// ids: one with an ST-Link's ids is an ST-Link, and a CMSIS-DAP probe keeps its family and wire.
    #[test]
    fn a_probe_is_given_its_family_by_its_ids() {
        let unknown_st_link = ProbeSeen {
            vendor_id: 0x0483,
            product_id: 0x374e,
            serial: Some("003A00000000000000000001".to_owned()),
            product: Some("STLINK-V3".to_owned()),
            wire: "bulk-v2",
        };
        let (attached, _) =
            assemble(vec![cmsis_dap(Some("E6614103E7176B2F")), unknown_st_link], Vec::new(), Vec::new(), &[]);
        assert_eq!(attached[0].facts.family, Some("cmsis-dap"));
        assert_eq!(attached[0].facts.wire, Some("bulk-v2"));
        assert_eq!(attached[0].id, "E6614103E7176B2F", "a probe is named by its serial number");
        assert_eq!(attached[1].facts.family, Some("st-link"));
        assert_eq!(attached[1].facts.wire, None);
    }

    /// **A PROBE NAMES THE SERIAL PORTS IT CARRIES**, matched by its USB serial number whatever the
    /// case of either spelling, and each such port is marked as the probe's rather than as a board.
    /// A port nobody's probe carries is named by no probe.
    #[test]
    fn a_probe_names_the_serial_ports_it_carries() {
        let ports = vec![
            port("COM7", Some("066dff555051897267143327")),
            port("COM9", Some("SOMEBOARD0001")),
        ];
        let passed_over = vec![declined(0x0483, 0x374b, "066DFF555051897267143327", "STM32 STLink")];
        let (attached, _) = assemble(vec![cmsis_dap(None)], passed_over, Vec::new(), &ports);

        let st_link = attached.iter().find(|one| one.facts.family == Some("st-link")).expect("the ST-Link");
        assert_eq!(st_link.facts.ports, ["COM7"]);
        let unnamed = attached.iter().find(|one| one.facts.family == Some("cmsis-dap")).expect("the probe");
        assert_eq!(unnamed.id, "? (2e8a:000c, no serial)", "a probe with no serial says it cannot be named");
        assert!(unnamed.facts.ports.is_empty(), "with no serial, no port is its");

        let com7 = attached.iter().find(|one| one.facts.port.as_deref() == Some("COM7")).expect("COM7");
        assert!(
            com7.description.contains("[the UART bridge of probe 066DFF555051897267143327: target not identified]"),
            "{}",
            com7.description
        );
        let com9 = attached.iter().find(|one| one.facts.port.as_deref() == Some("COM9")).expect("COM9");
        assert!(!com9.description.contains("UART bridge"), "{}", com9.description);
        assert_eq!(com9.facts.usb, Some((0x1fc9, 0x0143)));
        assert_eq!(com9.facts.serial.as_deref(), Some("SOMEBOARD0001"));
    }

    /// **A RUN CHOOSES AMONG THE PROBES THE LISTING SHOWS**, each with its serial, its ids and the
    /// serial ports it carries -- and nothing that is not a probe.
    #[test]
    fn a_run_is_offered_the_probes_the_listing_shows() {
        let ports = vec![port("COM7", Some("066dff555051897267143327")), port("COM9", Some("SOMEBOARD0001"))];
        let passed_over = vec![declined(0x0483, 0x374b, "066DFF555051897267143327", "STM32 STLink")];
        let (attached, _) = assemble(vec![cmsis_dap(Some("E6614103E7176B2F"))], passed_over, Vec::new(), &ports);

        let probes = probes_among(attached);
        assert_eq!(probes.len(), 2, "the two probes, and neither port: {probes:?}");
        assert_eq!(probes[0].serial.as_deref(), Some("E6614103E7176B2F"));
        assert_eq!(probes[0].usb, (0x2e8a, 0x000c));
        assert!(probes[0].ports.is_empty());
        assert_eq!(probes[1].serial.as_deref(), Some("066DFF555051897267143327"));
        assert_eq!(probes[1].description, "ST-LINK/V2-1");
        assert_eq!(probes[1].ports, ["COM7"]);
    }

    /// **THE JSON CARRIES EVERY ROW, WITH THE WORDS .NET LISTS AND THE IDS A PROGRAM SELECTS ON.**
    /// A schema version; each row's id, kind, type, status and description; its USB ids as four
    /// hex digits; a detail it does not have as `null`; and the devices passed over.
    #[test]
    fn the_json_carries_each_rows_words_and_ids() {
        let mut probe = row("E6614103E7176B2F", Kind::Probe, "Debugprobe on Pico (CMSIS-DAP)");
        probe.facts = Facts {
            family: Some("cmsis-dap"),
            wire: Some("bulk-v2"),
            usb: Some((0x2e8a, 0x000c)),
            serial: Some("E6614103E7176B2F".to_owned()),
            product: Some("Debugprobe on Pico (CMSIS-DAP)".to_owned()),
            port: None,
            ports: vec!["COM12".to_owned()],
        };
        let attached = [probe, row("COM3", Kind::Device, "COM3  (not a USB port)")];
        let passed_over = [declined(0x1915, 0x520f, "NOTAPROBE01", "a radio dongle")];

        let text = render_json(&attached, &[None, None], &passed_over);
        let document: Value = serde_json::from_str(&text).expect("the JSON parses");
        assert_eq!(document["schema"], 1);
        let first = &document["devices"][0];
        assert_eq!(first["id"], "E6614103E7176B2F");
        assert_eq!(first["kind"], "probe");
        assert_eq!(first["type"], "Device");
        assert_eq!(first["status"], "Connected");
        assert_eq!(first["description"], "Debugprobe on Pico (CMSIS-DAP)");
        assert_eq!(first["family"], "cmsis-dap");
        assert_eq!(first["wire"], "bulk-v2");
        assert_eq!(first["vid"], "2e8a");
        assert_eq!(first["pid"], "000c");
        assert_eq!(first["serial"], "E6614103E7176B2F");
        assert_eq!(first["ports"], json!(["COM12"]));
        let second = &document["devices"][1];
        assert_eq!(second["kind"], "device");
        assert!(second["vid"].is_null() && second["family"].is_null(), "an absent detail is null: {second}");
        assert_eq!(second["ports"], json!([]));
        assert_eq!(document["passed_over"][0]["vid"], "1915");
        assert_eq!(document["passed_over"][0]["interface"], "ST-Link Debug");
        assert!(text.ends_with("}\n"), "one document, ended by a newline");
    }

    /// **ONE ROW READS THE SAME IN EVERY FORM.** An `--identify` answer is the status, and the board
    /// it answered as is the description, in the table and the JSON alike; unasked, a row is
    /// attached.
    #[test]
    fn an_answer_reads_the_same_in_the_table_and_the_json() {
        let attached = [row("COM8", Kind::Device, "COM8  USB Serial Device  (2e8a:000a)")];
        let answer = Answer { board: Some("Raspberry Pi Pico 2 W".to_owned()), status: "Online", detail: String::new() };
        let answers = [Some(&answer)];
        let table = render(&attached, &answers);
        let document: Value = serde_json::from_str(&render_json(&attached, &answers, &[])).expect("JSON");
        assert!(table.contains("Online") && table.contains("Raspberry Pi Pico 2 W"), "{table}");
        assert_eq!(document["devices"][0]["status"], "Online");
        assert_eq!(document["devices"][0]["description"], "Raspberry Pi Pico 2 W");

        let unasked: Value = serde_json::from_str(&render_json(&attached, &[None], &[])).expect("JSON");
        assert_eq!(unasked["devices"][0]["status"], ATTACHED);
        assert_eq!(unasked["devices"][0]["description"], "COM8  USB Serial Device  (2e8a:000a)");
    }

    /// **EVERY CHARACTER MSBUILD OR XML TREATS SPECIALLY IS ESCAPED**, and a control character
    /// becomes a space.
    #[test]
    fn an_attribute_escapes_every_character_msbuild_or_xml_treats_specially() {
        assert_eq!(
            attribute("Raspberry Pi Debug Probe (CMSIS-DAP) ; 50% $(not) @(this) 'single' \"double\" <lt> &amp ? *"),
            "Raspberry Pi Debug Probe (CMSIS-DAP) %3B 50%25 %24(not) %40(this) %27single%27 &quot;double&quot; \
             &lt;lt&gt; &amp;amp %3F %2A"
        );
        assert_eq!(attribute("a;b"), "a%3Bb", "an id is escaped like any other value");
        assert_eq!(attribute("tab\there\nnull\u{0}end"), "tab here null end");
        assert_eq!(attribute("plain-ASCII_0123 (ok)"), "plain-ASCII_0123 (ok)", "the rest passes through");
    }

    /// **THE MSBUILD FORM's PROBES ARE NAMED BY THEIR SERIAL NUMBERS.** One item per probe with a
    /// serial number, under the target an SDK calls; the three metadata .NET reads beside the probe's
    /// kind, family and USB ids; never `RuntimeIdentifier`; no drive and no probe that cannot be
    /// named; a device of no identity the SDK is offered is left out of both targets; and a comment
    /// XML can read.
    #[test]
    fn the_msbuild_form_lists_the_probes_named_by_their_serial_numbers() {
        let mut probe = row("E6614103E7176B2F", Kind::Probe, "Debugprobe on Pico (CMSIS-DAP)");
        probe.facts = Facts {
            family: Some("cmsis-dap"),
            usb: Some((0x2e8a, 0x000c)),
            serial: Some("E6614103E7176B2F".to_owned()),
            ..Facts::default()
        };
        let mut st_link = row("066DFF555051897267143327", Kind::Probe, "ST-LINK/V2-1");
        st_link.facts = Facts {
            family: Some("st-link"),
            usb: Some((0x0483, 0x374b)),
            serial: Some("066DFF555051897267143327".to_owned()),
            ..Facts::default()
        };
        let attached = [
            probe,
            st_link,
            row("? (2e8a:000c, no serial)", Kind::Probe, "Debugprobe on Pico (CMSIS-DAP)"),
            row("COM8", Kind::Device, "COM8  USB Serial Device  (2e8a:000a)"),
            row("D:\\", Kind::Drive, "RP2350  HALTED in its bootloader, waiting for an image"),
        ];
        let text = render_msbuild(&attached);
        assert_eq!(
            text,
            "<Project>\n\
             \x20 <!-- Written by the lamella devices command: what is attached to this computer. -->\n\
             \x20 <PropertyGroup>\n\
             \x20   <LamellaDevicesSchema>1</LamellaDevicesSchema>\n\
             \x20 </PropertyGroup>\n\
             \x20 <Target Name=\"LamellaProbes\" Returns=\"@(LamellaProbe)\">\n\
             \x20   <ItemGroup>\n\
             \x20     <LamellaProbe Include=\"E6614103E7176B2F\" Type=\"Device\" Status=\"Connected\" \
             Description=\"Debugprobe on Pico (CMSIS-DAP)\" Kind=\"probe\" Family=\"cmsis-dap\" Usb=\"2e8a:000c\" />\n\
             \x20     <LamellaProbe Include=\"066DFF555051897267143327\" Type=\"Device\" Status=\"Connected\" \
             Description=\"ST-LINK/V2-1\" Kind=\"probe\" Family=\"st-link\" Usb=\"0483:374b\" />\n\
             \x20   </ItemGroup>\n\
             \x20 </Target>\n\
             \x20 <Target Name=\"LamellaDevices\" Returns=\"@(LamellaDevice)\">\n\
             \x20   <ItemGroup>\n\
             \x20   </ItemGroup>\n\
             \x20 </Target>\n\
             </Project>\n"
        );
        assert!(!text.contains("RuntimeIdentifier"), "dotnet run would apply it to the build");
        let comment_body = text.split("<!--").nth(1).and_then(|rest| rest.split("-->").next()).expect("a comment");
        assert!(!comment_body.contains("--"), "XML forbids -- inside a comment: {comment_body}");
    }

    /// **THE SDK IS OFFERED EACH DEVICE BY ITS USB IDENTITY, AND NOTHING IS ASKED.** On a bench of a
    /// board running Lamella firmware on its own USB and three probes, one of them an MCU-Link with
    /// two ports under one serial number: the board by its own id and product string, each probe's
    /// bridge by the id `--device` takes and marked as a bridge whose target is not identified, and
    /// neither a generic USB-UART nor a port with no USB identity.
    #[test]
    fn the_msbuild_form_offers_the_devices_by_their_usb_identity_alone() {
        let serial_port = |name: &str, usb: Option<(u16, u16)>, serial: Option<&str>, product: Option<&str>| {
            lamella_wire_host::SerialPortDesc {
                port: name.to_owned(),
                vid: usb.map(|(vendor_id, _)| vendor_id),
                pid: usb.map(|(_, product_id)| product_id),
                serial_number: serial.map(str::to_owned),
                product: product.map(str::to_owned),
            }
        };
        let mcu_link = ProbeSeen {
            vendor_id: 0x1fc9,
            product_id: 0x0143,
            serial: Some("PROBEALPHA001".to_owned()),
            product: Some("MCU-LINK CMSIS-DAP".to_owned()),
            wire: "bulk-v2",
        };
        let board = BoardSeen {
            vendor_id: lamella_wire::usb::VID,
            product_id: lamella_wire::usb::PID,
            serial: Some("E6614103E7176B2F".to_owned()),
            product: Some("Lamella Link (RP2350)".to_owned()),
        };
        let ports = [
            serial_port("COM75", Some((0x2e8a, 0x000c)), Some("E6640000000000A1"), Some("USB Serial Device (COM75)")),
            serial_port("COM7", Some((0x0483, 0x374b)), Some("066DFF555051897267143327"), Some("STMicroelectronics STLink Virtual COM Port (COM7)")),
            serial_port("COM11", Some((0x1fc9, 0x0143)), Some("PROBEALPHA001"), None),
            serial_port("COM76", Some((0x1fc9, 0x0143)), Some("PROBEALPHA001"), None),
            serial_port("COM5", Some((0x0403, 0x6001)), Some("A10KXYZ"), Some("USB Serial Port (COM5)")),
            serial_port("COM1", None, None, Some("Communications Port (COM1)")),
        ];
        let passed_over = vec![declined(0x0483, 0x374b, "066DFF555051897267143327", "STM32 STLink")];
        let (attached, _) = assemble(vec![cmsis_dap(Some("E6640000000000A1")), mcu_link], passed_over, vec![board], &ports);

        let text = render_msbuild(&attached);
        let devices = text.split("  <Target Name=\"LamellaDevices\"").nth(1).expect("the devices' target");
        assert_eq!(
            devices,
            " Returns=\"@(LamellaDevice)\">\n\
             \x20   <ItemGroup>\n\
             \x20     <LamellaDevice Include=\"usb:39e9:0001:E6614103E7176B2F\" Type=\"Device\" Status=\"Connected\" \
             Description=\"Lamella Link (RP2350)\" />\n\
             \x20     <LamellaDevice Include=\"serial:E6640000000000A1\" Type=\"Device\" Status=\"Connected\" \
             Description=\"COM75  USB Serial Device (COM75)  (2e8a:000c)  [the UART bridge of probe E6640000000000A1: \
             target not identified]\" Bridge=\"true\" />\n\
             \x20     <LamellaDevice Include=\"serial:066DFF555051897267143327\" Type=\"Device\" Status=\"Connected\" \
             Description=\"COM7  STMicroelectronics STLink Virtual COM Port (COM7)  (0483:374b)  [the UART bridge of \
             probe 066DFF555051897267143327: target not identified]\" Bridge=\"true\" />\n\
             \x20     <LamellaDevice Include=\"COM11\" Type=\"Device\" Status=\"Connected\" Description=\"COM11  (1fc9:0143)  \
             [the UART bridge of probe PROBEALPHA001: target not identified]  [serial PROBEALPHA001 names 2 ports, so the \
             port name addresses this one]\" Bridge=\"true\" />\n\
             \x20     <LamellaDevice Include=\"COM76\" Type=\"Device\" Status=\"Connected\" Description=\"COM76  (1fc9:0143)  \
             [the UART bridge of probe PROBEALPHA001: target not identified]  [serial PROBEALPHA001 names 2 ports, so the \
             port name addresses this one]\" Bridge=\"true\" />\n\
             \x20   </ItemGroup>\n\
             \x20 </Target>\n\
             </Project>\n"
        );
        let probes: Vec<&str> = text.lines().filter(|line| line.contains("<LamellaProbe ")).collect();
        assert_eq!(probes.len(), 3, "the three probes are still offered as probes: {text}");
    }

    /// **THE FILE IS WRITTEN WHOLE, AND A SECOND WRITE REPLACES THE FIRST**, with no file of its own
    /// left beside it.
    #[test]
    fn the_listing_file_is_written_whole_and_replaced() {
        let directory = std::env::temp_dir().join(format!("lamella-devices-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a scratch directory");
        let path = directory.join("lamella-devices.xml");
        write_whole(&path, "first\n").expect("the first write");
        write_whole(&path, "second\n").expect("the second write replaces it");
        assert_eq!(std::fs::read_to_string(&path).expect("read back"), "second\n");
        let left: Vec<_> = std::fs::read_dir(&directory).expect("list").filter_map(Result::ok).collect();
        assert_eq!(left.len(), 1, "no temporary file is left behind: {left:?}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// **AN OPTION THAT WOULD CHANGE NOTHING IS REFUSED, NOT IGNORED**, and each refusal names the
    /// verb. The three forms are told apart.
    #[test]
    fn the_forms_refuse_the_options_that_cannot_go_with_them() {
        let parse = |words: &[&str]| {
            let words: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
            args::parse(&words, &spec(&words)).expect("the words parse")
        };
        assert_eq!(form(&parse(&[])), Ok(Form::Table));
        assert_eq!(form(&parse(&["--identify", "COM8", "--all-devices"])), Ok(Form::Table));
        assert_eq!(form(&parse(&["--json"])), Ok(Form::Json));
        assert_eq!(form(&parse(&["--json", "--identify", "serial:E6640000000000A1"])), Ok(Form::Json));
        assert_eq!(form(&parse(&["--msbuild", "obj/devices.xml"])), Ok(Form::MsBuild("obj/devices.xml")));
        for refused in [
            &["--json", "--msbuild", "x.xml"][..],
            &["--json", "--all-devices"][..],
            &["--msbuild", "x.xml", "--all-devices"][..],
            &["--msbuild", "x.xml", "--identify", "COM8"][..],
            &["--identify", "--json"][..],
        ] {
            let refusal = form(&parse(refused)).expect_err("refused");
            assert!(refusal.starts_with("lamella devices: "), "{refused:?}: {refusal}");
        }
        let alone = ["--msbuild".to_owned()];
        assert!(args::parse(&alone, &spec(&alone)).is_err(), "--msbuild wants a path");
        let alone = ["--identify".to_owned()];
        assert!(args::parse(&alone, &spec(&alone)).is_err(), "--identify wants the device to ask");
    }

    /// **THE SDK'S OPTION IS INTERNAL**: the usage and its help never name it, the options a
    /// mistyped line is told of leave it out, and it is still taken when it is given.
    #[test]
    fn the_msbuild_form_is_taken_when_given_and_never_named() {
        assert!(!USAGE.contains("msbuild"), "{USAGE}");
        let mistyped = ["--jsn".to_owned()];
        let refusal = args::parse(&mistyped, &spec(&mistyped)).expect_err("an unknown option");
        assert!(!refusal.contains("msbuild"), "{refusal}");
        let given = ["--msbuild".to_owned(), "obj/devices.xml".to_owned()];
        let parsed = args::parse(&given, &spec(&given)).expect("taken when given");
        assert_eq!(form(&parsed), Ok(Form::MsBuild("obj/devices.xml")));
    }
}
