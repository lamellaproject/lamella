//! Complete project device lists, shared by the SDK and editor. Nothing is opened but an idle Raspberry Pi
//! Debug Probe, read at its debug port for the chip behind it, so a board's list leaves out a probe wired to
//! another chip.

use super::*;
use std::collections::HashSet;

/// Internal route arguments carry RHU's route, board, machine and availability. RuntimeIdentifier
/// remains the SDK's property. Only an emulator needs the last two fields.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Probe(String),
    Emulator { board: String, machine: String, available: bool },
}

#[derive(Debug)]
pub(super) struct Request<'a> {
    framework: &'a str,
    board: Option<&'a str>,
    emulator_board: bool,
    qualified: bool,
    routes: Vec<Route>,
}

impl<'a> Request<'a> {
    pub(super) fn parse(options: &'a args::Options, form: &Form<'_>) -> Result<Option<Self>, String> {
        let Some(framework) = options.value("--framework") else {
            if options.value("--board").is_some() || options.value("--route").is_some() || options.flag("--qualified") || options.flag("--emulator-board") {
                return Err("A project device list needs --framework.".to_owned());
            }
            return Ok(None);
        };
        let framework = if framework.eq_ignore_ascii_case("lamella1.0") {
            "lamella1.0"
        } else if framework.eq_ignore_ascii_case("net11.0") {
            "net11.0"
        } else {
            return Err(format!("The framework \"{framework}\" has no Lamella device list."));
        };
        if matches!(form, Form::Table) || options.value(IDENTIFY).is_some() || options.flag("--all-devices") {
            return Err("A project device list needs --json or --msbuild and never identifies a device.".to_owned());
        }
        let board = options.value("--board").filter(|board| !board.is_empty());
        if framework == "lamella1.0" {
            if let Some(board) = board {
                args::known_board("devices", board)?;
            }
            if options.value("--route").is_some() || options.flag("--qualified") || options.flag("--emulator-board") {
                return Err("RHU routes need the net11.0 framework.".to_owned());
            }
        }
        let routes = options.values("--route").filter_map(|value| {
            let parts: Vec<&str> = value.split(':').collect();
            let part_board = |value: &str| if value.is_empty() { board.unwrap_or_default().to_owned() } else { value.to_owned() };
            match parts.as_slice() {
                ["probe", id] => Some(Ok(Route::Probe(part_board(id)))),
                ["emulator", id, machine, available] => {
                    (!machine.is_empty()).then(|| Ok(Route::Emulator { board: part_board(id), machine: (*machine).to_owned(), available: available.eq_ignore_ascii_case("true") }))
                }
                [kind, ..] if !matches!(*kind, "probe" | "emulator") => None,
                _ => Some(Err(format!("The device route \"{value}\" is not recognized."))),
            }
        }).collect::<Result<Vec<_>, _>>()?;
        Ok(Some(Self { framework, board, emulator_board: options.flag("--emulator-board"),
            qualified: options.flag("--qualified"), routes }))
    }

    pub(super) fn list(&self, attached: &[Attached]) -> Listing {
        let mut listing = Listing::default();
        let catalog = Catalog::new();
        if self.framework == "lamella1.0" {
            if self.board.is_none() {
                listing.devices.push(Device {
                    id: "local".to_owned(), description: "This computer, under Lamella's interpreter".to_owned(),
                    device_type: "Local", model: INTERPRETER, ..Device::default()
                });
            }
            for one in attached.iter().filter(|one| one.kind == Kind::Device && one.id.is_some()) {
                let bridge = is_probe_bridge(one.facts.usb);
                if bridge {
                    listing.bridges.push(one.id.clone().unwrap());
                    if self.board.is_some() { continue; }
                } else if !names_lamella_firmware(one.facts.usb) {
                    continue;
                }
                let own_boards = board_named_by_usb(one).map(|name| catalog.named(name)).unwrap_or_default();
                if self.board.is_some_and(|wanted| !own_boards.is_empty() && !own_boards.iter().any(|own| wanted == own.id)) { continue; }
                listing.devices.push(Device {
                    id: one.id.clone().unwrap(), description: described(one, None), device_type: DEVICE_TYPE,
                    status: ATTACHED, model: INTERPRETER,
                    board: if own_boards.len() == 1 { own_boards[0].id.to_owned() } else { String::new() },
                    ..Device::default()
                });
            }
            if let Some(board) = self.board
                && crate::flash::links_native_image(board)
            {
                listing.probes(attached, &catalog, board, Reach::Named);
            }
        } else {
            let reach = if self.board.is_none() { Reach::Unknown } else if self.qualified { Reach::Qualified } else { Reach::Named };
            // An emulator runs the board it is listed for, so a project that names none lists it once for each board.
            let qualified = self.qualified || self.board.is_none();
            for route in &self.routes {
                let board = match route { Route::Probe(board) | Route::Emulator { board, .. } => board };
                if board.is_empty() || self.board.is_some_and(|wanted| board != wanted) { continue; }
                match route {
                    Route::Probe(board) => listing.probes(attached, &catalog, board, reach),
                    Route::Emulator { machine, available: true, .. } if self.board.is_none() || self.emulator_board => listing.devices.push(Device {
                        id: if qualified { format!("emulator:{machine}/{board}") } else { format!("emulator:{machine}") },
                        description: if qualified {
                            format!("{machine} emulator for {}: deploy as native (AOT)", crate::own_lines::board_name(board))
                        } else { format!("{machine} emulator: deploy as native (AOT)") },
                        device_type: "Emulator", status: "Installed", model: NATIVE, board: board.clone(),
                        route: "emulator", machine: machine.clone(), ..Device::default()
                    }),
                    _ => {}
                }
            }
        }
        let mut listed = HashSet::new();
        listing.devices.retain(|device| listed.insert(device.id.to_ascii_lowercase()));
        listing
    }
}

struct Board {
    id: &'static str,
    name: String,
    usb_name: Option<&'static str>,
}

impl Board {
    fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(self.usb_name)
    }
}

struct Catalog(Vec<Board>);

impl Catalog {
    fn new() -> Self {
        Self(lamella_catalog::BOARDS.iter().map(|(id, _)| Board {
            id, name: crate::own_lines::board_name(id),
            usb_name: lamella_catalog::load_board(id).and_then(|board| u16::try_from(board.product_model).ok())
                .and_then(lamella_wire_host::board_name),
        }).collect())
    }

    fn named(&self, name: &str) -> Vec<&Board> {
        self.0.iter().filter(|board| board.names().any(|known| known.eq_ignore_ascii_case(name)) || board.id == name).collect()
    }

    /// A probe's own product string can name its board. A shared family name, such as BBC micro:bit,
    /// admits each catalog board with that name; it does not choose a revision for the user.
    fn probe_allows(&self, product: Option<&str>, target: &str) -> bool {
        let Some(product) = product else { return true };
        let lower = product.to_ascii_lowercase();
        let stem = lower.split("cmsis-dap").next().unwrap_or(&lower).trim_end_matches([' ', '-', '(']);
        if stem.is_empty() { return true; }
        let exact = self.named(stem);
        if !exact.is_empty() { return exact.iter().any(|board| board.id == target); }
        let prefix = |value: &str, name: &str| value.strip_prefix(name).is_some_and(|rest| rest.starts_with([' ', '(', '-']));
        let mut matches: Vec<(&Board, usize)> = self.0.iter().flat_map(|board| board.names()
            .filter(|name| prefix(stem, &name.to_ascii_lowercase())).map(move |name| (board, name.len()))).collect();
        if let Some(longest) = matches.iter().map(|(_, length)| *length).max() {
            matches.retain(|(_, length)| *length == longest);
        } else {
            matches = self.0.iter().filter(|board| board.names().any(|name| prefix(&name.to_ascii_lowercase(), stem)))
                .map(|board| (board, 0)).collect();
        }
        matches.is_empty() || matches.iter().any(|(board, _)| board.id == target)
    }
}

/// The chip a board is built around, as its BSP spells it: its `family` and `part`.
fn chip_of_board(board: &str) -> Option<(String, String)> {
    let table = lamella_catalog::load_board(board)?;
    Some((lamella_catalog::family_of(&table)?, lamella_catalog::load_part(&table)?.part))
}

/// Whether the chip a read named can be the board's: the same family, and the same part where the read
/// named one. An RP2350's part takes the system bus to read, so at the debug port its family decides.
fn chip_fits(chip: &lamella_flash_routes::behind_probe::Chip, family: &str, part: &str) -> bool {
    chip.family == family && chip.part.is_none_or(|named| named == part)
}

#[derive(Default)]
struct Device {
    id: String,
    description: String,
    device_type: &'static str,
    status: &'static str,
    model: &'static str,
    board: String,
    serial: String,
    route: &'static str,
    machine: String,
    boards: Vec<String>,
    /// For a probe, the chip behind it, where the listing read it.
    chip: Option<lamella_flash_routes::behind_probe::Chip>,
    /// For a probe the listing tried to read, whether another program held it.
    in_use: Option<bool>,
}

#[derive(Default)]
pub(super) struct Listing {
    devices: Vec<Device>,
    bridges: Vec<String>,
    unreached: Vec<String>,
}

/// How a probe's row is named, by what the project says of its board.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// The project names the board: the probe's own id.
    Named,
    /// The board came from the device the person chose: the id they chose, `probe:<serial>/<board>`.
    Qualified,
    /// The project names no board: the probe's own id, once, with every board it reaches.
    Unknown,
}

impl Listing {
    /// The probes `board`'s image can be written through: those of the family its run goes through, and on a board
    /// with a debugger soldered to it, only that debugger's kind, as the run itself takes one. A probe cannot say
    /// which board it is wired to, so its row names none; for a project that names none, each probe is listed once,
    /// with the boards it reaches, and the deploy asks which.
    fn probes(&mut self, attached: &[Attached], catalog: &Catalog, board: &str, reach: Reach) {
        let Some(run) = crate::image_run::run_probe(board) else {
            if !self.unreached.iter().any(|id| id == board) { self.unreached.push(board.to_owned()); }
            return;
        };
        let board_chip = chip_of_board(board);
        for one in attached.iter().filter(|one| one.kind == Kind::Probe && one.id.is_some()) {
            if one.facts.family != Some(run.family)
                || run.usb.is_some_and(|usb| one.facts.usb != Some(usb))
                || !catalog.probe_allows(one.facts.product.as_deref(), board)
            { continue; }
            // A probe wired to another chip leaves the board's list. One in use, or whose chip is not known, stays.
            if let (Some(Behind::Chip(chip)), Some((family, part))) = (&one.facts.behind, &board_chip)
                && !chip_fits(chip, family, part)
            { continue; }
            let Some(serial) = &one.facts.serial else { continue };
            let id = one.id.as_deref().unwrap();
            if reach == Reach::Unknown
                && let Some(listed) = self.devices.iter_mut().find(|device| device.id.eq_ignore_ascii_case(id))
            {
                if !listed.boards.iter().any(|reached| reached == board) { listed.boards.push(board.to_owned()); }
                continue;
            }
            self.devices.push(Device {
                id: if reach == Reach::Qualified { format!("{id}/{board}") } else { id.to_owned() },
                description: NATIVE_DESCRIPTION.to_owned(),
                device_type: DEVICE_TYPE, status: ATTACHED, model: NATIVE,
                board: if reach == Reach::Unknown { String::new() } else { board.to_owned() },
                boards: if reach == Reach::Unknown { vec![board.to_owned()] } else { Vec::new() },
                serial: serial.clone(), route: "probe",
                chip: match &one.facts.behind { Some(Behind::Chip(chip)) => Some(chip.clone()), _ => None },
                in_use: one.facts.behind.as_ref().map(|behind| *behind == Behind::InUse),
                ..Device::default()
            });
        }
    }

    pub(super) fn json(&self) -> String {
        let devices: Vec<Value> = self.devices.iter().map(|device| json!({
            "id": device.id, "description": device.description, "type": device.device_type,
            "status": device.status, "model": device.model, "board": device.board,
            "serial": device.serial, "route": device.route, "machine": device.machine,
            "boards": device.boards.iter().map(|board| json!({ "id": board, "name": crate::own_lines::board_name(board) }))
                .collect::<Vec<_>>(),
            "chip": device.chip.as_ref().map(|chip| json!({
                "family": chip.family, "part": chip.part, "id": chip.chip_id.map(|id| format!("{id:016X}")),
            })),
            "in_use": device.in_use,
        })).collect();
        format!("{}\n", serde_json::to_string_pretty(&json!({
            "schema": 1, "devices": devices, "bridges": self.bridges, "unreached": self.unreached,
        })).unwrap())
    }

    pub(super) fn msbuild(&self) -> String {
        let mut text = "<Project>\n  <PropertyGroup><LamellaProjectDevicesSchema>1</LamellaProjectDevicesSchema></PropertyGroup>\n  <Target Name=\"LamellaDevices\" Returns=\"@(LamellaDevice)\">\n    <ItemGroup>\n".to_owned();
        for device in &self.devices {
            let _ = write!(text, "      <LamellaDevice Include=\"{}\"", attribute(&device.id));
            for (name, value) in [
                ("Description", device.description.as_str()), ("Type", device.device_type), ("Status", device.status),
                ("Model", device.model), ("Board", &device.board), ("Serial", &device.serial),
                ("Route", device.route), ("Machine", &device.machine),
            ] {
                if !value.is_empty() { let _ = write!(text, " {name}=\"{}\"", attribute(value)); }
            }
            // A list, as MSBuild splits one: each board escaped, and the semicolons between them left as they are.
            if !device.boards.is_empty() {
                let boards: Vec<String> = device.boards.iter().map(|board| attribute(board)).collect();
                let _ = write!(text, " Boards=\"{}\"", boards.join(";"));
            }
            if let Some(chip) = &device.chip {
                let _ = write!(text, " ChipFamily=\"{}\"", attribute(chip.family));
                if let Some(part) = chip.part {
                    let _ = write!(text, " ChipPart=\"{}\"", attribute(part));
                }
                if let Some(id) = chip.chip_id {
                    let _ = write!(text, " ChipId=\"{id:016X}\"");
                }
            }
            if let Some(in_use) = device.in_use {
                let _ = write!(text, " InUse=\"{in_use}\"");
            }
            text.push_str(" />\n");
        }
        text.push_str("    </ItemGroup>\n  </Target>\n");
        for (name, ids) in [("LamellaBridges", &self.bridges), ("LamellaUnreachedBoards", &self.unreached)] {
            let _ = writeln!(text, "  <Target Name=\"{name}\" Returns=\"@({name})\">\n    <ItemGroup>");
            for id in ids { let _ = writeln!(text, "      <{name} Include=\"{}\" />", attribute(id)); }
            text.push_str("    </ItemGroup>\n  </Target>\n");
        }
        text.push_str("</Project>\n");
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attached(id: Option<&str>, kind: Kind, usb: (u16, u16), product: &str) -> Attached {
        Attached {
            id: id.map(str::to_owned), kind, description: product.to_owned(),
            facts: Facts {
                usb: Some(usb), product: Some(product.to_owned()),
                serial: id.map(|id| id.strip_prefix("probe:").unwrap_or(id).to_owned()),
                family: (kind == Kind::Probe).then_some(if usb.0 == 0x0483 { ST_LINK } else { CMSIS_DAP }),
                ..Facts::default()
            },
        }
    }

    fn link(id: &str, board: &str) -> Attached {
        attached(Some(id), Kind::Device, (lamella_wire::usb::VID, lamella_wire::usb::PID),
            &if board.is_empty() { "Lamella Link".to_owned() } else { format!("Lamella Link ({board})") })
    }

    fn probe(id: &str, product: &str) -> Attached {
        attached(Some(id), Kind::Probe, (0x2e8a, 0x000c), product)
    }

    fn listed(options: &[&str], attached: &[Attached]) -> Listing {
        let args: Vec<String> = options.iter().map(|word| (*word).to_owned()).collect();
        let parsed = args::parse(&args, &spec(&args)).unwrap();
        Request::parse(&parsed, &form(&parsed).unwrap()).unwrap().unwrap().list(attached)
    }

    fn ids(list: &Listing) -> Vec<&str> {
        list.devices.iter().map(|device| device.id.as_str()).collect()
    }

    #[test]
    fn no_board_lists_local_first_then_every_selectable_link_and_bridge() {
        let bench = [
            probe("probe:GENERIC", "Debug Probe (CMSIS-DAP)"),
            link("usb:MATCH", "Pimoroni Pico Plus 2"),
            attached(Some("usb-serial:BRIDGE"), Kind::Device, (0x2e8a, 0x000c), "Debug Probe"),
            link("usb:OTHER", "Raspberry Pi Pico 2 W"),
            attached(Some("usb-serial:OTHERPORT"), Kind::Device, (0x0403, 0x6001), "USB serial"),
            attached(None, Kind::Device, (lamella_wire::usb::VID, lamella_wire::usb::PID), "Lamella Link"),
        ];
        let list = listed(&["--framework", "lamella1.0", "--json"], &bench);
        assert_eq!(ids(&list), ["local", "usb:MATCH", "usb-serial:BRIDGE", "usb:OTHER"]);
        assert_eq!(list.devices[1].description, "Pimoroni Pico Plus 2: deploy to interpreter");
        assert_eq!(list.devices[2].description, "Unknown board behind a debug probe: deploy to interpreter");
        assert_eq!(list.bridges, ["usb-serial:BRIDGE"]);
        assert_eq!(ids(&listed(&["--framework", "lamella1.0", "--json"], &[])), ["local"]);
    }

    #[test]
    fn framework_spelling_keeps_msbuilds_case_insensitive_choices() {
        let bench = [link("usb:MATCH", "Pimoroni Pico Plus 2")];
        for framework in ["lamella1.0", "LAMELLA1.0", "LaMeLlA1.0"] {
            assert_eq!(ids(&listed(&["--framework", framework, "--json"], &bench)), ["local", "usb:MATCH"]);
            assert_eq!(ids(&listed(&["--framework", framework, "--board", "pimoroni-pico-plus-2", "--json"], &bench)), ["usb:MATCH"]);
        }
        for framework in ["net11.0", "NET11.0", "Net11.0"] {
            let options = ["--framework", framework, "--json", "--route", "emulator:raspberrypi-pico-2:mps2-an505:true"];
            assert_eq!(ids(&listed(&options, &[])), ["emulator:mps2-an505/raspberrypi-pico-2"]);
            let mut named = options.to_vec();
            named.extend(["--board", "raspberrypi-pico-2"]);
            assert!(listed(&named, &[]).devices.is_empty(), "a physical target still hides the emulator");
        }
    }

    #[test]
    fn a_named_board_filters_known_other_links_and_probes_without_inventing_an_identity() {
        let bench = [
            link("usb:MATCH", "Pimoroni Pico Plus 2"),
            link("usb:OTHER", "Raspberry Pi Pico 2 W"),
            link("usb:UNKNOWN", "Fixture Board Alpha"),
            link("usb:UNNAMED", ""),
            attached(Some("usb-serial:BRIDGE"), Kind::Device, (0x2e8a, 0x000c), "Debug Probe"),
            probe("probe:MICROBIT", "BBC micro:bit CMSIS-DAP"),
            probe("probe:GENERIC", "Debug Probe (CMSIS-DAP)"),
            attached(Some("probe:STLINK"), Kind::Probe, (0x0483, 0x374b), "STM32 STLink"),
            attached(None, Kind::Probe, (0x2e8a, 0x000c), "Debug Probe (CMSIS-DAP)"),
        ];
        let options = ["--framework", "lamella1.0", "--board", "pimoroni-pico-plus-2", "--json"];
        let list = listed(&options, &bench);
        assert_eq!(ids(&list), ["usb:MATCH", "usb:UNKNOWN", "usb:UNNAMED", "probe:GENERIC"]);
        assert_eq!(list.devices[1].description, "Fixture Board Alpha: deploy to interpreter");
        assert_eq!(list.devices[2].description, "Unknown board running Lamella: deploy to interpreter");
        assert_eq!(list.devices[3].description, "Debug probe, Unknown board: deploy as native (AOT)");
        assert_eq!(list.devices[3].serial, "GENERIC");
        assert!(ids(&listed(&options, &[])).is_empty());
    }

    /// **A PROJECT's LIST READS NO LANGUAGE FROM A BOARD's USB STRING**, whatever follows the board's name
    /// there: the string is for people. A board whose firmware runs Python names its board alone, as a C#
    /// firmware does, so a C# project is offered it by that name, and a deploy's HELLO refuses it before
    /// anything is sent.
    #[test]
    fn a_projects_list_reads_no_language_from_a_boards_usb_string() {
        let bench = [
            link("usb:MATCH", "Pimoroni Pico Plus 2"),
            link("usb:PYTHON", "Raspberry Pi Pico 2 W"),
            link("usb:NOTED", "Raspberry Pi Pico 2 W, Python"),
        ];
        let all = ["local", "usb:MATCH", "usb:PYTHON", "usb:NOTED"];
        assert_eq!(ids(&listed(&["--framework", "lamella1.0", "--json"], &bench)), all);
        let its_own = ["--framework", "lamella1.0", "--board", "raspberrypi-pico-2-w", "--json"];
        assert_eq!(ids(&listed(&its_own, &bench)), ["usb:PYTHON", "usb:NOTED"]);
        let named = ["--framework", "lamella1.0", "--board", "pimoroni-pico-plus-2", "--json"];
        assert!(!ids(&listed(&named, &bench)).contains(&"usb:PYTHON"), "a known board is another board's");
    }

    #[test]
    fn a_board_without_a_lamella_native_image_gets_no_native_probe() {
        let bench = [attached(Some("probe:STLINK"), Kind::Probe, (0x0483, 0x374b), "STM32 STLink")];
        let list = listed(&["--framework", "lamella1.0", "--board", "st-nucleo-l476rg", "--json"], &bench);
        assert!(list.devices.is_empty());
    }

    /// A board whose native image is written through its own ST-LINK lists that ST-LINK.
    #[test]
    fn an_stm32_board_with_a_lamella_native_image_lists_its_st_link() {
        let bench = [attached(Some("probe:STLINK"), Kind::Probe, (0x0483, 0x374b), "STM32 STLink")];
        let list = listed(&["--framework", "lamella1.0", "--board", "st-stm32f769i-disco", "--json"], &bench);
        assert_eq!(ids(&list), ["probe:STLINK"]);
        assert!(list.devices[0].description.ends_with("deploy as native (AOT)"), "{}", list.devices[0].description);
    }

    #[test]
    fn filtering_recognizes_the_firmwares_human_name_when_it_differs_from_the_catalog_product() {
        let bench = [link("usb:ST-BOARD", "STM32F769I Discovery")];
        let pico = listed(&["--framework", "lamella1.0", "--board", "raspberrypi-pico-2", "--json"], &bench);
        assert!(pico.devices.is_empty(), "a named ST board is not a Pico");
        let st = listed(&["--framework", "lamella1.0", "--board", "st-stm32f769i-disco", "--json"], &bench);
        assert_eq!(st.devices[0].board, "st-stm32f769i-disco");
    }

    #[test]
    fn rhu_routes_choose_probe_families_and_installed_emulators_in_route_order() {
        let bench = [
            probe("probe:DAP", "Debug Probe (CMSIS-DAP)"),
            attached(Some("probe:STLINK"), Kind::Probe, (0x0483, 0x374b), "STM32 STLink"),
            link("usb:LINK", "Pimoroni Pico Plus 2"),
        ];
        let list = listed(&["--framework", "net11.0", "--json",
            "--route", "probe:raspberrypi-pico-2", "--route", "probe:raspberrypi-pico-2",
            "--route", "probe:st-stm32f769i-disco",
            "--route", "emulator:raspberrypi-pico-2:mps2-an505:True",
            "--route", "emulator:raspberrypi-pico-2:missing:false",
            "--route", "probe:raspberrypi-pico",
            "--route", "probe:not-in-catalog",
        ], &bench);
        assert_eq!(ids(&list), ["probe:DAP", "probe:STLINK", "emulator:mps2-an505/raspberrypi-pico-2"]);
        assert_eq!(list.devices[0].description, "Debug probe, Unknown board: deploy as native (AOT)");
        assert_eq!(list.devices[0].boards, ["raspberrypi-pico-2"]);
        assert_eq!(list.devices[1].boards, ["st-stm32f769i-disco"]);
        assert_eq!(list.devices[2].description, "mps2-an505 emulator for Raspberry Pi Pico 2: deploy as native (AOT)");
        assert_eq!(list.devices[2].machine, "mps2-an505");
        assert_eq!(list.unreached, ["raspberrypi-pico", "not-in-catalog"]);
        assert!(!list.msbuild().contains("RuntimeIdentifier"));
    }

    #[test]
    fn a_microbit_project_lists_only_a_microbits_own_daplink() {
        let bench = [
            probe("probe:PICOPROBE", "Debug Probe (CMSIS-DAP)"),
            attached(Some("probe:MICROBIT"), Kind::Probe, lamella_cmsis_dap_nrf::MICROBIT_DAPLINK, "BBC micro:bit CMSIS-DAP"),
            attached(Some("probe:STLINK"), Kind::Probe, (0x0483, 0x374b), "STM32 STLink"),
        ];
        for board in ["bbc-micro-bit-v1", "bbc-micro-bit-v2"] {
            let list = listed(&["--framework", "lamella1.0", "--board", board, "--json"], &bench);
            assert_eq!(ids(&list), ["probe:MICROBIT"], "{board}");
            assert_eq!(list.devices[0].board, board);
            assert_eq!(list.devices[0].serial, "MICROBIT");
            assert_eq!(list.devices[0].description, NATIVE_DESCRIPTION);
            assert!(list.unreached.is_empty(), "{board}: {:?}", list.unreached);
        }
        let pico = listed(&["--framework", "lamella1.0", "--board", "raspberrypi-pico-2", "--json"], &bench);
        assert_eq!(ids(&pico), ["probe:PICOPROBE"], "a micro:bit's DAPLink is not a Pico 2's probe");
    }

    #[test]
    fn a_boardless_project_lists_each_probe_once_as_an_unknown_board_with_the_boards_it_reaches() {
        let bench = [
            probe("probe:DAP", "Debug Probe (CMSIS-DAP)"),
            attached(Some("probe:STLINK"), Kind::Probe, (0x0483, 0x374b), "STM32 STLink"),
        ];
        let list = listed(&["--framework", "net11.0", "--json",
            "--route", "probe:raspberrypi-pico-2", "--route", "probe:pimoroni-pico-plus-2",
            "--route", "probe:st-stm32f769i-disco", "--route", "probe:pimoroni-pico-lipo-2-xl-w",
            "--route", "probe:raspberrypi-pico-2",
        ], &bench);
        assert_eq!(ids(&list), ["probe:DAP", "probe:STLINK"]);
        assert_eq!(list.devices[0].boards, ["raspberrypi-pico-2", "pimoroni-pico-plus-2", "pimoroni-pico-lipo-2-xl-w"]);
        assert_eq!(list.devices[1].boards, ["st-stm32f769i-disco"]);
        let names: Vec<String> = Catalog::new().0.into_iter().map(|board| board.name).collect();
        for device in &list.devices {
            assert_eq!(device.description, "Debug probe, Unknown board: deploy as native (AOT)");
            assert!(device.board.is_empty(), "{}", device.id);
            assert!(!names.iter().any(|name| device.description.contains(name.as_str())), "{}", device.description);
        }
        let json: Value = serde_json::from_str(&list.json()).unwrap();
        assert_eq!(json["devices"][1]["boards"],
            json!([{ "id": "st-stm32f769i-disco", "name": crate::own_lines::board_name("st-stm32f769i-disco") }]));
        let xml = list.msbuild();
        assert!(xml.contains(" Boards=\"raspberrypi-pico-2;pimoroni-pico-plus-2;pimoroni-pico-lipo-2-xl-w\""), "{xml}");
    }

    #[test]
    fn shared_emulator_machines_keep_each_board_and_only_deduplicate_the_same_route() {
        let list = listed(&["--framework", "net11.0", "--json",
            "--route", "emulator:raspberrypi-pico-2:mps2-an505:true",
            "--route", "emulator:pimoroni-pico-plus-2:mps2-an505:true",
            "--route", "emulator:raspberrypi-pico-2:mps2-an505:true",
        ], &[]);
        assert_eq!(ids(&list), ["emulator:mps2-an505/raspberrypi-pico-2", "emulator:mps2-an505/pimoroni-pico-plus-2"]);
        for (device, name) in list.devices.iter().zip(["Raspberry Pi Pico 2", "Pimoroni Pico Plus 2"]) {
            assert_eq!(device.machine, "mps2-an505");
            assert!(device.description.contains(name), "{}", device.description);
        }
        let xml = list.msbuild();
        assert!(xml.contains("Include=\"emulator:mps2-an505/raspberrypi-pico-2\""));
        assert!(xml.contains("Include=\"emulator:mps2-an505/pimoroni-pico-plus-2\""));
    }

    #[test]
    fn named_emulators_are_qualified_only_when_requested() {
        let mut options = vec!["--framework", "net11.0", "--board", "arm-mps2-an505", "--emulator-board", "--json",
            "--route", "emulator::mps2-an505:true"];
        assert_eq!(ids(&listed(&options, &[])), ["emulator:mps2-an505"]);
        assert_eq!(listed(&options, &[]).devices[0].description, "mps2-an505 emulator: deploy as native (AOT)");
        options.push("--qualified");
        assert_eq!(ids(&listed(&options, &[])), ["emulator:mps2-an505/arm-mps2-an505"]);
        let unavailable = listed(&["--framework", "net11.0", "--board", "arm-mps2-an505", "--emulator-board", "--json",
            "--route", "emulator::mps2-an505:false"], &[]);
        assert!(unavailable.devices.is_empty());
    }

    #[test]
    fn a_named_rhu_board_lists_no_emulator_even_when_its_route_names_a_machine() {
        let bench = [probe("probe:DAP", "Debug Probe (CMSIS-DAP)")];
        let options = ["--framework", "net11.0", "--board", "raspberrypi-pico-2", "--json",
            "--route", "probe:", "--route", "probe:raspberrypi-pico-2-w", "--route", "emulator:raspberrypi-pico-2-w:other:true",
            "--route", "emulator::mps2-an505:true"];
        let list = listed(&options, &bench);
        assert_eq!(ids(&list), ["probe:DAP"]);
        assert_eq!(list.devices[0].board, "raspberrypi-pico-2");
        assert_eq!(list.devices[0].description, NATIVE_DESCRIPTION);
        let mut options = options.to_vec();
        options.push("--qualified");
        let qualified = listed(&options, &bench);
        assert_eq!(ids(&qualified), ["probe:DAP/raspberrypi-pico-2"]);
        assert_eq!(qualified.devices[0].description, NATIVE_DESCRIPTION);
        assert!(qualified.devices[0].boards.is_empty());
    }

    #[test]
    fn probe_names_preserve_ambiguous_revisions_and_choose_the_longest_catalog_name() {
        let catalog = Catalog::new();
        assert!(!catalog.probe_allows(Some("BBC micro:bit CMSIS-DAP"), "raspberrypi-pico-2"));
        for board in ["bbc-micro-bit-v1", "bbc-micro-bit-v2"] {
            assert!(catalog.probe_allows(Some("BBC micro:bit CMSIS-DAP"), board));
        }
        assert!(catalog.probe_allows(Some("Raspberry Pi Pico 2 CMSIS-DAP"), "raspberrypi-pico-2"));
        assert!(!catalog.probe_allows(Some("Raspberry Pi Pico 2 CMSIS-DAP"), "raspberrypi-pico"));
        assert!(!catalog.probe_allows(Some("Raspberry Pi Pico 2 W USB (CMSIS-DAP)"), "raspberrypi-pico-2"));
        assert!(catalog.probe_allows(Some("Raspberry Pi Pico 2 W USB (CMSIS-DAP)"), "raspberrypi-pico-2-w"));
        for product in [None, Some("CMSIS-DAP"), Some("Debugprobe on Pico (CMSIS-DAP)"), Some("Raspberry Pi Picowhatever CMSIS-DAP")] {
            assert!(catalog.probe_allows(product, "raspberrypi-pico-2"));
        }
    }

    #[test]
    fn finished_json_and_msbuild_keep_the_same_words_metadata_and_literal_ids() {
        let serial = "probe:spaces & ' $(@item);?*\"";
        let list = listed(&["--framework", "net11.0", "--board", "raspberrypi-pico-2", "--json", "--route", "probe:"],
            &[probe(serial, "Debug Probe (CMSIS-DAP)")]);
        let json: Value = serde_json::from_str(&list.json()).unwrap();
        assert_eq!(json["schema"], 1);
        assert_eq!(json["devices"][0]["id"], serial);
        assert_eq!(json["devices"][0]["model"], "native");
        assert_eq!(json["devices"][0]["route"], "probe");
        let xml = list.msbuild();
        for (key, field) in [("Include", "id"), ("Description", "description"), ("Type", "type"),
            ("Status", "status"), ("Model", "model"), ("Serial", "serial"), ("Board", "board"), ("Route", "route")] {
            assert!(xml.contains(&format!("{key}=\"{}\"", attribute(json["devices"][0][field].as_str().unwrap()))), "{xml}");
        }
        assert!(!xml.contains("RuntimeIdentifier"));
        assert_eq!(json["devices"][0]["description"], NATIVE_DESCRIPTION);
    }

    #[test]
    fn unfamiliar_or_unavailable_rhu_routes_do_not_hide_valid_devices() {
        for route in ["future:raspberrypi-pico-2:any:true", "emulator:raspberrypi-pico-2::true", "emulator:raspberrypi-pico-2:machine:maybe"] {
            let list = listed(&["--framework", "net11.0", "--json",
                "--route", route, "--route", "probe:raspberrypi-pico-2",
                "--route", "emulator:arm-mps2-an505:mps2-an505:true"],
                &[probe("probe:TEST", "Debug Probe")]);
            assert_eq!(ids(&list), ["probe:TEST", "emulator:mps2-an505/arm-mps2-an505"], "{route}");
        }
    }

    #[test]
    fn a_project_list_refuses_identification_and_invalid_requests_before_enumeration() {
        for words in [
            vec!["--framework", "lamella1.0"],
            vec!["--framework", "lamella1.0", "--json", "--identify", "NEVER-OPEN"],
            vec!["--framework", "net11.0", "--json", "--route", "probe:board:extra"],
            vec!["--framework", "lamella1.0", "--json", "--route", "probe:raspberrypi-pico-2"],
            vec!["--framework", "other", "--json"],
            vec!["--msbuild", "unused", "--board", "raspberrypi-pico-2"],
        ] {
            let args: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
            let parsed = args::parse(&args, &spec(&args)).unwrap();
            assert!(Request::parse(&parsed, &form(&parsed).unwrap()).is_err(), "{args:?}");
        }
    }

    /// A Raspberry Pi Debug Probe as a project's list finds it once the chip behind it is read.
    fn read_probe(id: &str, behind: Behind) -> Attached {
        let mut one = probe(id, "Debug Probe (CMSIS-DAP)");
        one.facts.behind = Some(behind);
        one
    }

    /// What a read named.
    fn named(family: &'static str, part: Option<&'static str>) -> Behind {
        Behind::Chip(lamella_flash_routes::behind_probe::Chip { family, part, chip_id: None })
    }

    /// **A PROBE WIRED TO ANOTHER CHIP LEAVES THE BOARD'S LIST, AND ONE WHOSE CHIP IS NOT KNOWN STAYS.** A
    /// different family leaves, and a different part where the read named one; a probe in use, one whose
    /// read named nothing and one that was not read stay, for both frameworks. Each says what was read.
    #[test]
    fn a_probe_wired_to_another_chip_leaves_the_boards_list_and_one_not_known_stays() {
        let bench = [
            read_probe("probe:RP2040", named("rp2040", Some("rp2040"))),
            read_probe("probe:RP2350", named("rp2350", None)),
            read_probe("probe:RP2350A", named("rp2350", Some("rp2350a"))),
            read_probe("probe:RP2350B", named("rp2350", Some("rp2350b"))),
            read_probe("probe:HELD", Behind::InUse),
            read_probe("probe:SILENT", Behind::Unknown("no debug port answered".to_owned())),
            probe("probe:UNREAD", "Debug Probe (CMSIS-DAP)"),
        ];
        let kept = ["probe:RP2350", "probe:RP2350B", "probe:HELD", "probe:SILENT", "probe:UNREAD"];
        let board = "pimoroni-pico-plus-2";
        let list = listed(&["--framework", "lamella1.0", "--board", board, "--json"], &bench);
        assert_eq!(ids(&list), kept);
        let rhu = listed(&["--framework", "net11.0", "--board", board, "--route", &format!("probe:{board}"), "--json"], &bench);
        assert_eq!(ids(&rhu), kept, "net11.0's probe routes reach the same filter");

        let json: Value = serde_json::from_str(&list.json()).unwrap();
        let devices = &json["devices"];
        assert_eq!(devices[1]["chip"], json!({ "family": "rp2350", "part": "rp2350b", "id": null }));
        assert_eq!(devices[1]["in_use"], json!(false));
        assert_eq!((&devices[2]["chip"], &devices[2]["in_use"]), (&Value::Null, &json!(true)));
        assert_eq!((&devices[3]["chip"], &devices[3]["in_use"]), (&Value::Null, &json!(false)));
        assert_eq!((&devices[4]["chip"], &devices[4]["in_use"]), (&Value::Null, &Value::Null), "a probe not read says nothing");
        assert_eq!(devices[1]["description"], NATIVE_DESCRIPTION, "the words do not change");

        let xml = list.msbuild();
        let line = |id: &str| xml.lines().find(|line| line.contains(&format!("Include=\"{id}\""))).unwrap().to_owned();
        assert!(line("probe:RP2350B").contains(" ChipFamily=\"rp2350\" ChipPart=\"rp2350b\" InUse=\"false\""), "{xml}");
        assert!(line("probe:RP2350").contains(" ChipFamily=\"rp2350\" InUse=\"false\"") && !line("probe:RP2350").contains("ChipPart"), "{xml}");
        assert!(line("probe:HELD").contains(" InUse=\"true\"") && !line("probe:HELD").contains("Chip"), "{xml}");
        assert!(!line("probe:UNREAD").contains("Chip") && !line("probe:UNREAD").contains("InUse"), "{xml}");
    }

    /// **AN RP2040 BOARD KEEPS ONLY AN RP2040.** Its family has one part, so the read names it, and a probe
    /// wired to an RP2350 leaves.
    #[test]
    fn a_board_keeps_a_probe_whose_chip_fits_it_by_family_and_by_part() {
        let rp2040 = named("rp2040", Some("rp2040"));
        let Behind::Chip(rp2040) = rp2040 else { unreachable!() };
        assert!(chip_fits(&rp2040, "rp2040", "rp2040"));
        assert!(!chip_fits(&rp2040, "rp2350", "rp2350a"));
        let Behind::Chip(rp2350) = named("rp2350", None) else { unreachable!() };
        assert!(chip_fits(&rp2350, "rp2350", "rp2350a") && chip_fits(&rp2350, "rp2350", "rp2350b"));
        assert_eq!(chip_of_board("raspberrypi-pico"), Some(("rp2040".to_owned(), "rp2040".to_owned())));
        assert_eq!(chip_of_board("pimoroni-pico-plus-2"), Some(("rp2350".to_owned(), "rp2350b".to_owned())));
        assert_eq!(chip_of_board("no-such-board"), None);
    }
}
