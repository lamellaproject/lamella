//! What a launch asks for: the fields of a user's launch configuration, and the debug manifest the
//! project's build wrote, read in one place into the [`Plan`] a session starts from.

use lamella_dap::{Chosen, PathMap};
use serde_json::{Map, Value as Json};
use std::path::PathBuf;

/// Where the program runs, as the launch's device id names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Device {
    /// This computer's interpreter: the id `local`, in any case.
    Local,
    /// A board's own interpreter, over Lamella Link: a board's USB id, a serial port, or a network
    /// address, exactly as `dotnet run --device` takes it.
    Link(String),
    /// Native code on a board behind the debug probe with this serial number: the id `probe:` and
    /// the serial number.
    Probe(String),
}

/// What the build made, as the debug manifest says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An assembly for this computer's interpreter.
    Host,
    /// An image baked for a board's interpreter.
    Interpreted,
    /// A native image, written to the board through its probe.
    Native,
}

impl Kind {
    /// What the build made, in a reader's words.
    fn made(self) -> &'static str {
        match self {
            Kind::Host => "a program for this computer",
            Kind::Interpreted => "an image for a board's interpreter",
            Kind::Native => "a native image for a board behind a debug probe",
        }
    }
}

impl Device {
    /// The kind of image this device runs: the device decides, as it does for `dotnet run`.
    fn runs(&self) -> Kind {
        match self {
            Device::Local => Kind::Host,
            Device::Link(_) => Kind::Interpreted,
            Device::Probe(_) => Kind::Native,
        }
    }

    /// What this device runs, in a reader's words.
    fn runs_words(&self) -> &'static str {
        match self {
            Device::Local => "runs the program on this computer",
            Device::Link(_) => "runs the board's interpreter",
            Device::Probe(_) => "is a debug probe, which takes a native image",
        }
    }
}

/// A launch, read: everything a session starts from.
#[derive(Debug)]
pub struct Plan {
    /// Where the program runs.
    pub device: Device,
    /// The image the build made: the program for this computer, a baked image, or a native ELF.
    pub image: PathBuf,
    /// The program's Portable PDB: the one the manifest names, or else the one beside the image, where
    /// there is one.
    pub symbols: Option<PathBuf>,
    /// The baked image's line map, each of its methods' sequence points and local names: the one the
    /// manifest names, or else `<image>.srcmap.json` beside it, where there is one.
    pub line_map: Option<PathBuf>,
    /// The board the image was built for.
    pub board: Option<String>,
    /// The class library's folder, for a program run on this computer.
    pub references: Option<PathBuf>,
    /// Whether the program stops at the first line of `Main`.
    pub stop_at_entry: bool,
    /// Whether the program runs without debugging.
    pub no_debug: bool,
    /// Whether the session attaches to the program already on the board, writing nothing.
    pub attach: bool,
    /// The arguments `Main` is given.
    pub args: Vec<String>,
    /// How the source paths the build recorded in the program map to the ones on this computer: the
    /// manifest's `pathMap`, empty where the build rewrote no path.
    pub path_map: PathMap,
    /// Lines for the debug console: what the launch asked for that it is not given.
    pub notes: Vec<String>,
}

impl Plan {
    /// How the adapter runs the session this plan starts, with `backend` as its target: the plan's
    /// notes, then every line the start said about the session ([`say`]), for the Debug Console.
    pub fn chosen(self, backend: Box<dyn lamella_dap::DebugBackend>) -> Chosen {
        let mut notes = self.notes;
        notes.extend(take_said());
        Chosen {
            backend,
            stop_at_entry: self.stop_at_entry,
            no_debug: self.no_debug,
            notes,
            path_map: self.path_map,
        }
    }
}

thread_local! {
    /// The lines [`say`] was given since they were last taken, in the order said.
    static SAID: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Says `line` about the session being started, for its Debug Console: what the start could not do
/// as the launch asked, and what the session does instead. [`Plan::chosen`] hands it to the adapter
/// with the launch's own notes.
pub fn say(line: String) {
    SAID.with(|said| said.borrow_mut().push(line));
}

/// The lines said since they were last taken, in the order said, leaving none.
pub fn take_said() -> Vec<String> {
    SAID.with(|said| std::mem::take(&mut *said.borrow_mut()))
}

/// The fields a user's launch configuration has.
const USER_FIELDS: &[&str] = &["type", "request", "name", "projectPath", "device", "stopAtEntry", "args"];

/// The fields an editor writes into every launch of its own accord. VS Code's internal fields, which
/// all begin `__`, are passed over by that rule rather than listed.
const EDITOR_FIELDS: &[&str] = &[
    "noDebug",
    "preLaunchTask",
    "postDebugTask",
    "presentation",
    "internalConsoleOptions",
    "debugServer",
    "suppressMultipleSessionWarning",
];

/// The field the editor adds to name the debug manifest its build wrote. It begins `__`, so it never
/// reads as a field of a user's file, and a user who writes `manifest` is told it is no field.
const MANIFEST_FIELD: &str = "__manifest";

/// The debug manifest's format, as this debugger reads it.
const MANIFEST_FORMAT: u64 = 1;

/// Reads a launch -- the request's `command` (`launch` or `attach`) and its `arguments` -- and the
/// debug manifest it names, into the plan a session starts from.
///
/// A field the launch has that no Lamella launch has is not refused: it is ignored, and named in
/// [`Plan::notes`], so a misspelled field is seen rather than silently doing nothing.
///
/// # Errors
/// Why the launch cannot start, in words for the user: a field of the wrong kind, no device or no
/// manifest, a manifest that cannot be read, a manifest built for another device or board than the
/// one the launch names, or a native build that names no board for a launch that would write it.
pub fn plan(command: &str, arguments: &Json) -> Result<Plan, String> {
    let empty = Map::new();
    let fields = match arguments {
        Json::Object(fields) => fields,
        Json::Null => &empty,
        _ => return Err("the launch's arguments are not a JSON object".to_owned()),
    };
    let mut notes: Vec<String> = fields
        .keys()
        .filter(|name| {
            !name.starts_with("__")
                && !USER_FIELDS.contains(&name.as_str())
                && !EDITOR_FIELDS.contains(&name.as_str())
        })
        .map(|name| {
            format!(
                "The launch configuration's field \"{name}\" is not a field of a Lamella \
                 launch configuration, so it is ignored."
            )
        })
        .collect();
    notes.extend(text_note(fields, "type", Some("lamella")));
    notes.extend(request_note(fields, command));
    notes.extend(text_note(fields, "name", None));
    notes.extend(text_note(fields, "projectPath", None));
    let device_id = text(fields, "device")?
        .ok_or_else(|| {
            "the launch names no device. Choose the device to debug on, as dotnet run --device \
             takes it, or name it in the launch configuration's \"device\" field."
                .to_owned()
        })?
        .trim();
    let device = device_named(device_id)?;
    let stop_at_entry = flag(fields, "stopAtEntry")?;
    let no_debug = flag(fields, "noDebug")?;
    let args = strings(fields, "args")?;
    let manifest_path = text(fields, MANIFEST_FIELD)?.ok_or_else(|| {
        "the launch names no debug manifest, so there is no program to debug. The editor names the \
         one the project's build writes: start the session from the editor, which builds first."
            .to_owned()
    })?;
    let manifest = read_manifest(manifest_path)?;
    if manifest.kind != device.runs() {
        return Err(format!(
            "the build made {}, and the device {device_id} {}. Build the project for the device you \
             debug on.",
            manifest.kind.made(),
            device.runs_words()
        ));
    }
    if let Some(built_for) = &manifest.device {
        if !device_named(built_for).is_ok_and(|built| same_device(&built, &device)) {
            return Err(format!(
                "the build was made for the device {built_for}, and the launch names {device_id}. \
                 Build the project for the device you debug on."
            ));
        }
    }
    let rows = [
        lamella_probe_core::selection::board_named(device_id).map(|named| {
            (named, format!("the launch names {device_id}, the board {named} behind that probe"))
        }),
        manifest.device.as_deref().and_then(|row| {
            lamella_probe_core::selection::board_named(row).map(|named| {
                (named, format!("the build's device, {row}, names the board {named} behind that probe"))
            })
        }),
    ];
    match manifest.board.as_deref() {
        Some(built) => {
            if let Some((_, said)) = rows.iter().flatten().find(|(named, _)| !named.eq_ignore_ascii_case(built)) {
                return Err(format!(
                    "the build was made for the board {built}, and {said}. Build the project for the \
                     board you debug on."
                ));
            }
        }
        None if manifest.kind == Kind::Native && command != "attach" => {
            return Err(match rows.iter().flatten().next() {
                Some((named, said)) => format!(
                    "the build named no board, and {said}. A program is written for the board its \
                     build names, so nothing was written. Build the project for {named}."
                ),
                None => "the build named no board, and a program is written for the board its build \
                         names, so nothing was written. Build the project for the board you debug on."
                    .to_owned(),
            });
        }
        None => {}
    }
    let symbols = named_or_beside(
        manifest.symbols,
        "the symbols",
        manifest.image.with_extension("pdb"),
        manifest_path,
    )?;
    let line_map = named_or_beside(
        manifest.line_map,
        "the file that maps the program to its source",
        with_suffix(&manifest.image, ".srcmap.json"),
        manifest_path,
    )?;
    Ok(Plan {
        device,
        image: manifest.image,
        symbols,
        line_map,
        board: manifest.board,
        references: manifest.references,
        stop_at_entry,
        no_debug,
        attach: command == "attach",
        args,
        path_map: PathMap::new(manifest.path_map),
        notes,
    })
}

/// The device a device id names, as `dotnet run --device` takes the id: `local` in any case, as it
/// takes that one. The spaces around the id are not part of it, as the probe selector reads a serial
/// number and a board.
fn device_named(id: &str) -> Result<Device, String> {
    let id = id.trim();
    if id.eq_ignore_ascii_case("local") {
        return Ok(Device::Local);
    }
    let prefix = lamella_probe_core::selection::PROBE_ID_PREFIX;
    if id.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix)) {
        let serial = lamella_probe_core::selection::serial_named(id);
        if serial.is_empty() {
            return Err(format!(
                "the device {id} names no probe: a probe's id is \"probe:\" and its serial number, as \
                 the lamella devices command lists it."
            ));
        }
        return Ok(Device::Probe(serial.to_owned()));
    }
    if id.is_empty() {
        return Err("the launch's device is empty".to_owned());
    }
    Ok(Device::Link(id.to_owned()))
}

/// Whether `a` and `b` are one device: the same kind, and an id that differs at most in case.
fn same_device(a: &Device, b: &Device) -> bool {
    match (a, b) {
        (Device::Local, Device::Local) => true,
        (Device::Link(a), Device::Link(b)) | (Device::Probe(a), Device::Probe(b)) => {
            a.eq_ignore_ascii_case(b)
        }
        _ => false,
    }
}

/// The debug manifest, as the project's build wrote it.
struct Manifest {
    kind: Kind,
    image: PathBuf,
    symbols: Option<PathBuf>,
    line_map: Option<PathBuf>,
    board: Option<String>,
    device: Option<String>,
    references: Option<PathBuf>,
    /// Each prefix a source has on this computer, with the prefix the build recorded in its place.
    path_map: Vec<(String, String)>,
}

/// Reads the debug manifest at `path`.
fn read_manifest(path: &str) -> Result<Manifest, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read the debug manifest {path}: {error}"))?;
    let json: Json = serde_json::from_slice(&bytes)
        .map_err(|error| format!("the debug manifest {path} is not JSON: {error}"))?;
    let Json::Object(fields) = &json else {
        return Err(format!("the debug manifest {path} is not a JSON object"));
    };
    let in_manifest = |error: String| format!("the debug manifest {path}: {error}");
    match fields.get("format").and_then(Json::as_u64) {
        Some(MANIFEST_FORMAT) => {}
        _ => {
            return Err(format!(
                "the debug manifest {path} comes from a Lamella SDK this debugger does not read. The \
                 debugger that comes with the project's SDK reads the manifest that SDK writes."
            ));
        }
    }
    let kind = match text(fields, "kind").map_err(in_manifest)? {
        Some("host") => Kind::Host,
        Some("interpreted") => Kind::Interpreted,
        Some("native") => Kind::Native,
        Some(other) => {
            return Err(in_manifest(format!(
                "\"kind\" is \"{other}\", and a build makes \"host\", \"interpreted\" or \"native\""
            )));
        }
        None => return Err(in_manifest("it names no \"kind\"".to_owned())),
    };
    let path_of = |name: &str| -> Result<Option<PathBuf>, String> {
        Ok(text(fields, name).map_err(in_manifest)?.map(PathBuf::from))
    };
    let image = path_of("image")?.ok_or_else(|| in_manifest("it names no \"image\"".to_owned()))?;
    Ok(Manifest {
        kind,
        image,
        symbols: path_of("symbols")?,
        line_map: path_of("lineMap")?,
        board: text(fields, "board").map_err(in_manifest)?.map(str::to_owned),
        device: text(fields, "device").map_err(in_manifest)?.map(str::to_owned),
        references: path_of("references")?,
        path_map: path_map(fields).map_err(in_manifest)?,
    })
}

/// The manifest's `pathMap`: MSBuild's `PathMap` as the build applied it, a list of pairs, each the
/// prefix a source has on this computer (`from`) and the prefix the build recorded in its place
/// (`to`), in the order the build tries them. A manifest without one is a build that rewrote no path.
///
/// **A MAP THAT CANNOT BE READ REFUSES THE LAUNCH, NAMING IT.** Read as no map, every breakpoint in a
/// rewritten source would be resolved in a file the program does not record, and every frame shown
/// at a path the editor does not have.
fn path_map(fields: &Map<String, Json>) -> Result<Vec<(String, String)>, String> {
    let pairs = match fields.get("pathMap") {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(pairs)) => pairs,
        Some(other) => {
            return Err(format!(
                "\"pathMap\" is {other}, where a list of pairs, each a \"from\" and a \"to\", was expected"
            ));
        }
    };
    pairs
        .iter()
        .map(|pair| {
            let prefix = |side: &str| match pair.get(side) {
                Some(Json::String(prefix)) if !prefix.is_empty() => Some(prefix.clone()),
                _ => None,
            };
            match (pair.is_object(), prefix("from"), prefix("to")) {
                (true, Some(from), Some(to)) => Ok((from, to)),
                _ => Err(format!(
                    "\"pathMap\" holds {pair}, where each entry is a pair of paths, a \"from\" and a \
                     \"to\", neither of them empty"
                )),
            }
        })
        .collect()
}

/// The text field `name`, absent if the launch does not have it.
fn text<'a>(fields: &'a Map<String, Json>, name: &str) -> Result<Option<&'a str>, String> {
    match fields.get(name) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(text)) => Ok(Some(text)),
        Some(other) => Err(format!("\"{name}\" is {other}, where a text was expected")),
    }
}

/// The file the debug manifest names for `what`, or else `beside`, the one a build leaves beside the
/// image, where there is one; `None` where the manifest names none and none is there.
///
/// **A FILE THE BUILD NAMED IS NOT A SUGGESTION**, as the class library's folder is not: one that is
/// not there refuses the launch, naming it, because the build and what it left disagree and a session
/// served without the file would answer without what the build made for it. One the manifest does
/// not name is looked for beside the image, and a session goes without it where there is none.
fn named_or_beside(
    named: Option<PathBuf>,
    what: &str,
    beside: PathBuf,
    manifest: &str,
) -> Result<Option<PathBuf>, String> {
    match named {
        Some(file) if file.is_file() => Ok(Some(file)),
        Some(file) => Err(format!(
            "the debug manifest {manifest} names {what} {}, and there is no file there. Build the \
             project again.",
            file.display()
        )),
        None => Ok(beside.is_file().then_some(beside)),
    }
}

/// The refusal for `symbols` that the launch found and that are not a Portable PDB: the second half of
/// [`named_or_beside`]'s rule, said once for every tier that reads them. Served without them, a session
/// would answer every breakpoint as a line with no code and run every step to the step budget.
pub fn not_a_portable_pdb(symbols: &std::path::Path) -> String {
    format!(
        "the symbols {} could not be read as a Portable PDB, so the program cannot be mapped to its \
         source. Build the project again.",
        symbols.display()
    )
}

/// `path` with `suffix` after its whole file name: an image's line map is `<image>.srcmap.json`, beside
/// it, which is where the build writes one.
fn with_suffix(path: &std::path::Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// A note for the launch configuration's field `name`, which a launch reads as a text: where it holds
/// something else, or a text other than `expected`.
fn text_note(fields: &Map<String, Json>, name: &str, expected: Option<&str>) -> Option<String> {
    match fields.get(name) {
        None | Some(Json::Null) => None,
        Some(Json::String(text)) => expected.filter(|expected| text != expected).map(|expected| {
            format!(
                "The launch configuration's \"{name}\" is \"{text}\", and a Lamella \
                 launch configuration's is \"{expected}\"."
            )
        }),
        Some(other) => Some(format!(
            "The launch configuration's \"{name}\" is {other}, where a text was \
             expected."
        )),
    }
}

/// A note for a launch configuration whose `request` is not the request, `command`, that started
/// this session.
fn request_note(fields: &Map<String, Json>, command: &str) -> Option<String> {
    match fields.get("request") {
        Some(Json::String(request)) if request != command => Some(format!(
            "The launch configuration's \"request\" is \"{request}\", and this session \
             was started by a \"{command}\" request."
        )),
        _ => text_note(fields, "request", None),
    }
}

/// The true-or-false field `name`, false if the launch does not have it.
fn flag(fields: &Map<String, Json>, name: &str) -> Result<bool, String> {
    match fields.get(name) {
        None | Some(Json::Null) => Ok(false),
        Some(Json::Bool(value)) => Ok(*value),
        Some(other) => Err(format!("\"{name}\" is {other}, where true or false was expected")),
    }
}

/// The list-of-texts field `name`, empty if the launch does not have it.
fn strings(fields: &Map<String, Json>, name: &str) -> Result<Vec<String>, String> {
    match fields.get(name) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(items)) => items
            .iter()
            .map(|item| match item {
                Json::String(text) => Ok(text.clone()),
                other => Err(format!("\"{name}\" holds {other}, where only texts were expected")),
            })
            .collect(),
        Some(other) => Err(format!("\"{name}\" is {other}, where a list of texts was expected")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The scratch folder of the test `test`.
    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lamella-debug-launch-{}-{test}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        dir
    }

    /// A manifest written to a scratch file, named for the test that wrote it.
    fn manifest(test: &str, body: Json) -> String {
        let path = scratch(test).join("debug.json");
        std::fs::write(&path, body.to_string()).expect("the manifest is written");
        path.to_string_lossy().into_owned()
    }

    /// `name` in the scratch folder of `test`, written, so a manifest that names it names a file.
    fn written(test: &str, name: &str) -> PathBuf {
        let path = scratch(test).join(name);
        std::fs::write(&path, b"").expect("a file the manifest names");
        path
    }

    /// The image, symbols and line map an interpreted build names, written as the build leaves them.
    fn interpreted_files(test: &str) -> (PathBuf, PathBuf, PathBuf) {
        (written(test, "App.lmli"), written(test, "App.pdb"), written(test, "App.lmli.srcmap.json"))
    }

    fn interpreted(test: &str) -> String {
        let (image, symbols, line_map) = interpreted_files(test);
        manifest(
            test,
            json!({
                "format": 1, "kind": "interpreted", "image": image,
                "symbols": symbols, "lineMap": line_map,
                "board": "raspberrypi-pico-2-w", "device": "usb:39e9:0001:ABC"
            }),
        )
    }

    fn refused(arguments: Json) -> String {
        match plan("launch", &arguments) {
            Ok(plan) => panic!("{arguments} should have been refused, and planned {plan:?}"),
            Err(reason) => reason,
        }
    }

    #[test]
    fn a_launch_on_a_boards_usb_plans_its_interpreter_from_the_manifest() {
        let path = interpreted("usb");
        let plan = plan(
            "launch",
            &json!({
                "type": "lamella", "request": "launch", "name": "Debug on the board",
                "projectPath": "C:/p/Blinky.csproj", "device": "usb:39e9:0001:ABC",
                "stopAtEntry": true, "__manifest": path, "__sessionId": "x"
            }),
        )
        .expect("planned");
        let (image, _, line_map) = interpreted_files("usb");
        assert_eq!(plan.device, Device::Link("usb:39e9:0001:ABC".to_owned()));
        assert_eq!(plan.image, image);
        assert_eq!(plan.line_map, Some(line_map));
        assert_eq!(plan.board.as_deref(), Some("raspberrypi-pico-2-w"));
        assert!(plan.stop_at_entry && !plan.no_debug && !plan.attach);
        assert!(plan.notes.is_empty(), "every field there is known: {:?}", plan.notes);
    }

    #[test]
    fn a_field_no_lamella_launch_has_is_named_in_a_note_and_not_refused() {
        let path = interpreted("unknown");
        let plan = plan(
            "launch",
            &json!({ "device": "usb:39e9:0001:ABC", "__manifest": path, "stopOnEntery": true, "manifest": "x" }),
        )
        .expect("an unknown field does not stop a launch");
        assert_eq!(plan.notes.len(), 2, "{:?}", plan.notes);
        assert!(plan.notes.iter().any(|note| note.contains("\"stopOnEntery\"")));
        assert!(
            plan.notes.iter().any(|note| note.contains("\"manifest\"")),
            "a user's \"manifest\" is no field: the editor's is \"__manifest\""
        );
    }

    /// **WHAT THE START SAYS ABOUT THE SESSION REACHES ITS DEBUG CONSOLE**, after the launch's own
    /// notes and in the order said, and once: the next start begins with none.
    #[test]
    fn the_lines_a_start_says_reach_the_session_after_its_notes() {
        let path = interpreted("said");
        let plan = plan("launch", &json!({ "device": "usb:39e9:0001:ABC", "__manifest": path, "stopOnEntery": true }))
            .expect("an unknown field does not stop a launch");
        say(String::from("first"));
        say(String::from("second"));
        let chosen = plan.chosen(Box::new(crate::link::tests::NeverDriven));
        assert_eq!(chosen.notes.len(), 3, "{:?}", chosen.notes);
        assert!(chosen.notes[0].contains("\"stopOnEntery\""), "the plan's note first: {:?}", chosen.notes);
        assert!(chosen.notes[0].starts_with("The launch configuration's"), "{:?}", chosen.notes);
        assert_eq!(chosen.notes[1..], [String::from("first"), String::from("second")]);
        assert!(take_said().is_empty(), "handed on once");
    }

    #[test]
    fn the_device_decides_the_tier_and_a_build_for_another_is_refused_naming_both() {
        let native = manifest(
            "native",
            json!({ "format": 1, "kind": "native", "image": "C:/b/App.elf", "board": "bbc-micro-bit-v1" }),
        );
        let plan = plan("launch", &json!({ "device": "probe:E660C0D1A2B3C4D5", "__manifest": native }))
            .expect("a probe takes the native image");
        assert_eq!(plan.device, Device::Probe("E660C0D1A2B3C4D5".to_owned()));

        let reason = refused(json!({ "device": "usb:39e9:0001:ABC", "__manifest": native }));
        assert!(
            reason.contains("native image") && reason.contains("usb:39e9:0001:ABC") && reason.contains("interpreter"),
            "{reason}"
        );
    }

    #[test]
    fn a_probe_id_is_read_as_every_tool_reads_it_with_or_without_a_board() {
        for id in ["probe:E660C0D1A2B3C4D5", "PROBE:E660C0D1A2B3C4D5", "probe:E660C0D1A2B3C4D5/raspberrypi-pico-2-w"] {
            assert_eq!(device_named(id), Ok(Device::Probe("E660C0D1A2B3C4D5".to_owned())), "{id}");
        }
        assert_eq!(device_named("usb:39e9:0001:ABC"), Ok(Device::Link("usb:39e9:0001:ABC".to_owned())));
        assert_eq!(device_named("COM9"), Ok(Device::Link("COM9".to_owned())));
        assert_eq!(device_named("local"), Ok(Device::Local));
    }

    #[test]
    fn a_build_for_a_probes_board_row_is_for_the_probe_the_launch_names() {
        let native = manifest(
            "board-row",
            json!({
                "format": 1, "kind": "native", "image": "C:/b/App.elf", "board": "raspberrypi-pico-2-w",
                "device": "probe:E660C0D1A2B3C4D5/raspberrypi-pico-2-w"
            }),
        );
        let launched = plan("launch", &json!({ "device": "probe:e660c0d1a2b3c4d5", "__manifest": native }));
        assert!(launched.is_ok(), "{launched:?}");
    }

    /// **A PROBE'S ROW NAMES A BOARD, AND ONE THAT DISAGREES WITH THE BUILD'S IS ANOTHER DEVICE.** A
    /// probe reaches whichever board is wired to it, so its serial number alone matches; the board
    /// after the slash is the one the launch says it debugs, and two parts of one chip family take
    /// each other's image without a word.
    #[test]
    fn a_probe_row_that_names_another_board_than_the_build_is_refused_naming_both() {
        let row = manifest(
            "board-mismatch-row",
            json!({
                "format": 1, "kind": "native", "image": "C:/b/App.elf", "board": "raspberrypi-pico-2-w",
                "device": "probe:E660C0D1A2B3C4D5/raspberrypi-pico-2-w"
            }),
        );
        let reason = refused(json!({ "device": "probe:E660C0D1A2B3C4D5/raspberrypi-pico-2", "__manifest": row }));
        assert!(
            reason.contains("raspberrypi-pico-2-w") && reason.contains("probe:E660C0D1A2B3C4D5/raspberrypi-pico-2"),
            "{reason}"
        );
        let board = manifest(
            "board-mismatch-board",
            json!({ "format": 1, "kind": "native", "image": "C:/b/App.elf", "board": "raspberrypi-pico-2-w" }),
        );
        let reason = refused(json!({ "device": "probe:E660C0D1A2B3C4D5/raspberrypi-pico-2", "__manifest": board }));
        assert!(reason.contains("raspberrypi-pico-2-w"), "{reason}");
        for device in ["probe:E660C0D1A2B3C4D5/RaspberryPi-Pico-2-W", "probe:E660C0D1A2B3C4D5"] {
            assert!(plan("launch", &json!({ "device": device, "__manifest": board })).is_ok(), "{device}");
        }
    }

    /// **A ROW'S BOARD IS A LISTING'S LABEL, AND IT NAMES NO PART.** A native build that named no board
    /// is refused for a launch that would write it, before any probe is opened: the board an image is
    /// written for is its build's. Where a row names a board -- the launch's device, or the device the
    /// build was made for -- the refusal says to build for that board. An attach writes nothing, so it
    /// is planned.
    #[test]
    fn a_probe_rows_board_does_not_stand_in_for_a_build_that_named_none() {
        let unboarded = manifest("no-board", json!({ "format": 1, "kind": "native", "image": "C:/b/App.elf" }));
        let row = json!({ "device": "probe:E660C0D1A2B3C4D5/raspberrypi-pico-2-w", "__manifest": unboarded });
        let reason = refused(row.clone());
        assert!(reason.contains("Build the project for raspberrypi-pico-2-w"), "{reason}");
        assert!(plan("attach", &row).is_ok(), "an attach writes nothing");

        let listed = manifest(
            "no-board-listed",
            json!({
                "format": 1, "kind": "native", "image": "C:/b/App.dll",
                "device": "probe:E660C0D1A2B3C4D5/raspberrypi-pico-2-w"
            }),
        );
        for device in ["probe:E660C0D1A2B3C4D5/raspberrypi-pico-2-w", "probe:E660C0D1A2B3C4D5"] {
            let launch = json!({ "device": device, "__manifest": listed });
            let reason = refused(launch.clone());
            assert!(reason.contains("Build the project for raspberrypi-pico-2-w"), "{device}: {reason}");
            assert!(plan("attach", &launch).is_ok(), "{device}: an attach writes nothing");
        }

        let bare = manifest(
            "no-board-bare",
            json!({ "format": 1, "kind": "native", "image": "C:/b/App.dll", "device": "probe:E660C0D1A2B3C4D5" }),
        );
        let launch = json!({ "device": "probe:E660C0D1A2B3C4D5", "__manifest": bare });
        let reason = refused(launch.clone());
        assert!(
            reason.contains("the build named no board")
                && reason.contains("Build the project for the board you debug on"),
            "{reason}"
        );
        assert!(plan("attach", &launch).is_ok(), "an attach writes nothing");
    }

    /// **EVERY ROW THAT NAMES A BOARD NAMES THE BUILD'S.** The device the build was made for is a row
    /// too, and one that names another board than the build's makes the build disagree with itself,
    /// so the launch is refused, naming both, whichever row the launch names.
    #[test]
    fn the_device_a_build_was_made_for_names_the_builds_board_or_the_launch_is_refused() {
        let disagrees = manifest(
            "listed-other-board",
            json!({
                "format": 1, "kind": "native", "image": "C:/b/App.elf", "board": "raspberrypi-pico-2",
                "device": "probe:E660C0D1A2B3C4D5/raspberrypi-pico-2-w"
            }),
        );
        for device in ["probe:E660C0D1A2B3C4D5", "probe:E660C0D1A2B3C4D5/raspberrypi-pico-2"] {
            let reason = refused(json!({ "device": device, "__manifest": disagrees }));
            assert!(
                reason.contains("the board raspberrypi-pico-2,") && reason.contains("the board raspberrypi-pico-2-w"),
                "{device}: {reason}"
            );
        }
    }

    #[test]
    fn a_build_made_for_another_device_is_refused_naming_both() {
        let path = interpreted("other");
        let reason = refused(json!({ "device": "usb:39e9:0001:XYZ", "__manifest": path }));
        assert!(reason.contains("usb:39e9:0001:ABC") && reason.contains("usb:39e9:0001:XYZ"), "{reason}");
    }

    #[test]
    fn local_is_this_computer_and_its_program_takes_the_launchs_args() {
        let symbols = written("host", "App.pdb");
        let host = manifest(
            "host",
            json!({ "format": 1, "kind": "host", "image": "C:/b/App.dll", "symbols": symbols }),
        );
        let plan = plan("launch", &json!({ "device": "local", "__manifest": host, "args": ["a", "b c"] }))
            .expect("planned");
        assert_eq!(plan.device, Device::Local);
        assert_eq!(plan.args, vec!["a".to_owned(), "b c".to_owned()]);
    }

    /// **A FILE THE BUILD NAMED IS NOT A SUGGESTION.** One that is not there refuses the launch,
    /// naming it, whichever file it is -- before any device is reached.
    #[test]
    fn a_file_the_manifest_names_that_is_not_there_refuses_the_launch_naming_it() {
        let image = written("named-missing", "App.lmli");
        let missing = scratch("named-missing").join("Gone.pdb");
        let symbols = manifest(
            "named-missing",
            json!({ "format": 1, "kind": "interpreted", "image": image, "symbols": missing, "device": "usb:39e9:0001:ABC" }),
        );
        let reason = refused(json!({ "device": "usb:39e9:0001:ABC", "__manifest": symbols }));
        assert!(reason.contains("the symbols") && reason.contains("Gone.pdb"), "{reason}");

        let missing = scratch("named-missing").join("Gone.srcmap.json");
        let line_map = manifest(
            "named-missing",
            json!({ "format": 1, "kind": "interpreted", "image": image, "lineMap": missing, "device": "usb:39e9:0001:ABC" }),
        );
        let reason = refused(json!({ "device": "usb:39e9:0001:ABC", "__manifest": line_map }));
        assert!(
            reason.contains("the file that maps the program to its source") && reason.contains("Gone.srcmap.json"),
            "{reason}"
        );
    }

    /// A file the manifest does not name is looked for beside the image, where the build leaves it:
    /// the image's symbols with the image's stem, and its line map after the image's whole name. Where
    /// there is none, the session goes without it.
    #[test]
    fn a_file_the_manifest_does_not_name_is_looked_for_beside_the_image() {
        let image = written("beside", "App.lmli");
        let bare = manifest("beside", json!({ "format": 1, "kind": "interpreted", "image": image }));
        let without = plan("launch", &json!({ "device": "usb:39e9:0001:ABC", "__manifest": bare })).expect("planned");
        assert_eq!((without.symbols, without.line_map), (None, None), "nothing beside the image yet");

        let symbols = written("beside", "App.pdb");
        let line_map = written("beside", "App.lmli.srcmap.json");
        let found = plan("launch", &json!({ "device": "usb:39e9:0001:ABC", "__manifest": bare })).expect("planned");
        assert_eq!(found.symbols, Some(symbols));
        assert_eq!(found.line_map, Some(line_map));
    }

    /// A program's arguments are carried to every tier's plan, a board's as well as this computer's,
    /// and never refused or remarked on: a program takes arguments wherever it runs.
    #[test]
    fn args_are_carried_to_the_plan_on_every_tier_and_never_refused() {
        let path = interpreted("args-board");
        let native = manifest(
            "args-probe",
            json!({ "format": 1, "kind": "native", "image": "C:/b/App.elf", "board": "bbc-micro-bit-v1" }),
        );
        for (device, manifest) in [("usb:39e9:0001:ABC", &path), ("probe:E660C0D1A2B3C4D5", &native)] {
            let planned = plan(
                "launch",
                &json!({ "device": device, "__manifest": manifest, "args": ["a", "b c"] }),
            )
            .expect("planned");
            assert_eq!(planned.args, vec!["a".to_owned(), "b c".to_owned()], "{device}");
            assert!(planned.notes.is_empty(), "{device}: {:?}", planned.notes);
        }
    }

    /// The fields every launch configuration has are read for what they say, and one that says
    /// something no Lamella launch says is named in a note, as an unknown field is: VS Code routes a
    /// configuration here by its `type`, and another client may send anything.
    #[test]
    fn the_fields_every_launch_has_are_read_and_one_that_says_something_else_is_noted() {
        let path = interpreted("contract-fields");
        let noted = plan(
            "launch",
            &json!({
                "device": "usb:39e9:0001:ABC", "__manifest": path,
                "type": "cppdbg", "request": "attach", "projectPath": 7, "name": true
            }),
        )
        .expect("notes, not a refusal");
        for field in ["\"type\"", "\"request\"", "\"projectPath\"", "\"name\""] {
            assert!(noted.notes.iter().any(|note| note.contains(field)), "{field}: {:?}", noted.notes);
        }
        let clean = plan(
            "launch",
            &json!({
                "device": "usb:39e9:0001:ABC", "__manifest": path,
                "type": "lamella", "request": "launch", "projectPath": "C:/p/App.csproj", "name": "Debug"
            }),
        )
        .expect("planned");
        assert!(clean.notes.is_empty(), "{:?}", clean.notes);
    }

    /// A device id is read with the spaces around it trimmed, as the probe selector reads a serial
    /// number and a board, so a padded `local` is this computer, a padded probe row is that probe, and
    /// an id of spaces alone is empty. A launch names the trimmed id.
    #[test]
    fn a_device_id_is_read_with_the_spaces_around_it_trimmed() {
        assert_eq!(device_named(" local "), Ok(Device::Local));
        assert_eq!(
            device_named(" probe:BBBB2222/raspberrypi-pico-2 "),
            Ok(Device::Probe("BBBB2222".to_owned()))
        );
        assert!(device_named("   ").is_err_and(|reason| reason.contains("empty")), "spaces alone");

        let native = manifest(
            "padded-device",
            json!({ "format": 1, "kind": "native", "image": "C:/b/App.elf", "board": "raspberrypi-pico-2" }),
        );
        let planned = plan("launch", &json!({ "device": " probe:BBBB2222/raspberrypi-pico-2 ", "__manifest": native }))
            .expect("planned");
        assert_eq!(planned.device, Device::Probe("BBBB2222".to_owned()));
        let reason = refused(json!({ "device": " probe:BBBB2222/raspberrypi-pico-2-w ", "__manifest": native }));
        assert!(reason.contains("the launch names probe:BBBB2222/raspberrypi-pico-2-w,"), "{reason}");
    }

    /// `dotnet run --device` takes `local` in any case, and a launch's device is the id it takes.
    #[test]
    fn local_names_this_computer_in_any_case_as_dotnet_run_takes_it() {
        for id in ["local", "LOCAL", "Local"] {
            assert_eq!(device_named(id), Ok(Device::Local), "{id}");
        }
    }

    #[test]
    fn run_without_debugging_and_attach_are_read_from_the_request() {
        let path = interpreted("modes");
        let arguments = json!({ "device": "usb:39e9:0001:ABC", "__manifest": path, "noDebug": true });
        assert!(plan("launch", &arguments).expect("planned").no_debug);
        assert!(plan("attach", &arguments).expect("planned").attach);
    }

    #[test]
    fn a_launch_with_no_device_or_no_manifest_says_which_is_missing() {
        let path = interpreted("missing");
        assert!(refused(json!({ "__manifest": path })).contains("no device"));
        assert!(refused(json!({ "device": "local" })).contains("no debug manifest"));
        assert!(refused(json!({ "device": "probe:", "__manifest": path })).contains("names no probe"));
    }

    #[test]
    fn a_manifest_this_debugger_cannot_read_is_refused_naming_why() {
        let newer = manifest("newer", json!({ "format": 2, "kind": "host", "image": "C:/b/App.dll" }));
        let reason = refused(json!({ "device": "local", "__manifest": newer }));
        assert!(
            reason.contains("a Lamella SDK this debugger does not read")
                && reason.contains("The debugger that comes with the project's SDK")
                && !reason.contains("format 2"),
            "{reason}"
        );
        let no_image = manifest("no-image", json!({ "format": 1, "kind": "host" }));
        assert!(refused(json!({ "device": "local", "__manifest": no_image })).contains("\"image\""));
        assert!(refused(json!({ "device": "local", "__manifest": "Z:/nowhere/debug.json" })).contains("could not read"));
    }

    /// **THE BUILD'S PATH MAP IS THE SESSION'S**: each pair of the manifest's `pathMap` as the SDK
    /// writes it, a prefix on this computer and the prefix the build recorded in its place, in the
    /// order the build tries them. A manifest without one maps no path.
    #[test]
    fn the_manifests_path_map_is_the_sessions_in_the_order_the_build_gave_it() {
        let mapped = manifest(
            "path-map",
            json!({
                "format": 1, "kind": "native", "image": "C:/b/App.elf", "board": "raspberrypi-pico-2-w",
                "pathMap": [
                    { "from": "C:\\w\\App\\obj\\", "to": "/rhu/obj/App/" },
                    { "from": "C:\\w\\App\\", "to": "/rhu/src/App/" }
                ]
            }),
        );
        let planned = plan("launch", &json!({ "device": "probe:E660C0D1A2B3C4D5", "__manifest": mapped }))
            .expect("planned");
        assert_eq!(
            planned.path_map,
            PathMap::new(vec![
                (String::from("C:\\w\\App\\obj\\"), String::from("/rhu/obj/App/")),
                (String::from("C:\\w\\App\\"), String::from("/rhu/src/App/")),
            ])
        );
        let unmapped = interpreted("no-path-map");
        let planned = plan("launch", &json!({ "device": "usb:39e9:0001:ABC", "__manifest": unmapped }))
            .expect("planned");
        assert_eq!(planned.path_map, PathMap::default(), "a build that rewrote no path");
    }

    /// A `pathMap` that is not a list of pairs of paths refuses the launch, naming it and the manifest.
    /// Read as no map, a breakpoint in a rewritten source would be resolved in a file the program does
    /// not record.
    #[test]
    fn a_path_map_that_is_not_a_list_of_pairs_of_paths_is_refused_naming_it() {
        for (test, map) in [
            ("map-text", json!("C:\\w\\App=/rhu/src/App")),
            ("map-pair-list", json!([["C:\\w\\App\\", "/rhu/src/App/"]])),
            ("map-no-to", json!([{ "from": "C:\\w\\App\\" }])),
            ("map-empty-from", json!([{ "from": "", "to": "/rhu/src/App/" }])),
            ("map-number", json!([{ "from": "C:\\w\\App\\", "to": 7 }])),
        ] {
            let path = manifest(test, json!({ "format": 1, "kind": "host", "image": "C:/b/App.dll", "pathMap": map }));
            let reason = refused(json!({ "device": "local", "__manifest": path }));
            assert!(reason.contains("\"pathMap\"") && reason.contains(&path), "{test}: {reason}");
        }
    }

    #[test]
    fn a_field_of_the_wrong_kind_is_refused_naming_it() {
        let path = interpreted("kinds");
        assert!(refused(json!({ "device": "usb:39e9:0001:ABC", "__manifest": path, "stopAtEntry": "yes" })).contains("\"stopAtEntry\""));
        assert!(refused(json!({ "device": "local", "__manifest": path, "args": "a b" })).contains("\"args\""));
    }
}
