//! Complete project device lists, shared by the SDK and editor without opening a device.

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
            qualified: options.flag("--qualified") || board.is_none(), routes }))
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
                listing.probes(attached, &catalog, board, false);
            }
        } else {
            for route in &self.routes {
                let board = match route { Route::Probe(board) | Route::Emulator { board, .. } => board };
                if board.is_empty() || self.board.is_some_and(|wanted| board != wanted) { continue; }
                match route {
                    Route::Probe(board) => listing.probes(attached, &catalog, board, self.qualified),
                    Route::Emulator { machine, available: true, .. } if self.board.is_none() || self.emulator_board => listing.devices.push(Device {
                        id: if self.qualified { format!("emulator:{machine}/{board}") } else { format!("emulator:{machine}") },
                        description: if self.qualified {
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
}

#[derive(Default)]
pub(super) struct Listing {
    devices: Vec<Device>,
    bridges: Vec<String>,
    unreached: Vec<String>,
}

impl Listing {
    fn probes(&mut self, attached: &[Attached], catalog: &Catalog, board: &str, qualified: bool) {
        let families = crate::image_run::probe_families(board);
        if families.is_empty() {
            if !self.unreached.iter().any(|id| id == board) { self.unreached.push(board.to_owned()); }
            return;
        }
        for one in attached.iter().filter(|one| one.kind == Kind::Probe && one.id.is_some()) {
            if !one.facts.family.is_some_and(|family| families.contains(&family))
                || !catalog.probe_allows(one.facts.product.as_deref(), board)
            { continue; }
            let Some(serial) = &one.facts.serial else { continue };
            let id = one.id.as_deref().unwrap();
            self.devices.push(Device {
                id: if qualified { format!("{id}/{board}") } else { id.to_owned() },
                description: if qualified { format!("Debug probe, {}: deploy as native (AOT)", crate::own_lines::board_name(board)) }
                    else { NATIVE_DESCRIPTION.to_owned() },
                device_type: DEVICE_TYPE, status: ATTACHED, model: NATIVE,
                board: board.to_owned(), serial: serial.clone(), route: "probe", ..Device::default()
            });
        }
    }

    pub(super) fn json(&self) -> String {
        let devices: Vec<Value> = self.devices.iter().map(|device| json!({
            "id": device.id, "description": device.description, "type": device.device_type,
            "status": device.status, "model": device.model, "board": device.board,
            "serial": device.serial, "route": device.route, "machine": device.machine,
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
        assert_eq!(list.devices[2].description, "Board behind a debug probe: deploy to interpreter");
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
        assert_eq!(list.devices[2].description, "Board running Lamella: deploy to interpreter");
        assert_eq!(list.devices[3].description, "Debug probe: deploy as native (AOT)");
        assert_eq!(list.devices[3].serial, "GENERIC");
        assert!(ids(&listed(&options, &[])).is_empty());
    }

    #[test]
    fn a_board_without_a_lamella_native_image_gets_no_native_probe() {
        let bench = [attached(Some("probe:STLINK"), Kind::Probe, (0x0483, 0x374b), "STM32 STLink")];
        let list = listed(&["--framework", "lamella1.0", "--board", "st-stm32f769i-disco", "--json"], &bench);
        assert!(list.devices.is_empty());
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
            "--route", "probe:bbc-micro-bit-v2",
            "--route", "probe:not-in-catalog",
        ], &bench);
        assert_eq!(ids(&list), ["probe:DAP/raspberrypi-pico-2", "probe:STLINK/st-stm32f769i-disco", "emulator:mps2-an505/raspberrypi-pico-2"]);
        assert_eq!(list.devices[0].description, "Debug probe, Raspberry Pi Pico 2: deploy as native (AOT)");
        assert_eq!(list.devices[2].description, "mps2-an505 emulator for Raspberry Pi Pico 2: deploy as native (AOT)");
        assert_eq!(list.devices[2].machine, "mps2-an505");
        assert_eq!(list.unreached, ["bbc-micro-bit-v2", "not-in-catalog"]);
        assert!(!list.msbuild().contains("RuntimeIdentifier"));
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
        assert_eq!(ids(&listed(&options, &bench)), ["probe:DAP/raspberrypi-pico-2"]);
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
            assert_eq!(ids(&list), ["probe:TEST/raspberrypi-pico-2", "emulator:mps2-an505/arm-mps2-an505"], "{route}");
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
}
