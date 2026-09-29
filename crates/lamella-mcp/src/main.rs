//! The Lamella MCP server (canonical, native): exposes the Lamella toolchain -- compile / run / bake / size /
//! enumerate / deploy / Lamella Link SOURCE-LEVEL debug -- as MCP tools over stdio (JSON-RPC 2.0, newline-delimited),
//! linking the crates directly (no wasm round-trip; always the latest toolchain; one self-contained binary).
//! Hand-rolled on `serde_json` -- no tokio, no MCP SDK -- matching the workspace's dependency-minimal ethos.

use lamella_wire::{Capabilities, Negotiated, TransportError};
use lamella_wire_host::debug_backend::WireTransport;
use lamella_wire_host::engine::{
    CompileFailure, LcscCompiler, LoopbackLink, Outcome, Repl, ReplCompiler, install_host_clock,
};
#[cfg(feature = "bake")]
use lamella_wire_host::engine::BakedSerialLink;
use lamella_wire_host::{deployed_status_blocking, hello_blocking, list_serial, SerialTransport, UsbTransport};
#[cfg(feature = "bake")]
use lamella_wire_host::{TransferAck, board_name, deploy_image_blocking};
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::time::{Duration, Instant};

/// The default serial baud for a Lamella Link carrier (USB-CDC ignores it; a real UART wants it).
const BAUD: u32 = 115_200;

#[cfg(feature = "bake")]
mod debug;

mod schema;

/// The shared, canonical tool contract (name / description / inputSchema / annotations). Embedded so the binary
/// is self-contained; the browser host vendors a byte-identical copy. `tools/list` serves the `tools` array.
const CONTRACT: &str = include_str!("../tools.json");

/// The MCP RESOURCES: the chip registry, the C# surface and the samples, as stored text. The board
/// registry is the exception -- its body is computed by [`boards_resource_text`] from the wire's own
/// board table, so it cannot fall behind the boards this build knows.
const RESOURCES: &str = include_str!("../resources.json");

/// The managed corlib bytes the in-process host runner (`LoopbackLink`) executes against: `LAMELLA_CORLIB` if
/// set, else the committed dev fixture (mirrors `LcscCompiler::discover`).
fn corlib_bytes() -> Result<Vec<u8>, String> {
    if let Some(path) = std::env::var_os("LAMELLA_CORLIB") {
        return std::fs::read(&path).map_err(|error| format!("LAMELLA_CORLIB: {error}"));
    }
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../lamella-load/tests/fixtures/corlib.dll");
    std::fs::read(&fixture).map_err(|error| format!("corlib fixture {}: {error}", fixture.display()))
}

/// Every tool the contract declares, whatever this build routes, parsed once.
fn contract_tools() -> &'static [Value] {
    static TOOLS: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();
    TOOLS.get_or_init(|| {
        serde_json::from_str::<Value>(CONTRACT)
            .ok()
            .and_then(|contract| contract.get("tools").and_then(Value::as_array).cloned())
            .unwrap_or_default()
    })
}

/// The tools only a build with the `bake` feature routes. A build without it serves none of them.
const BAKE_ONLY: [&str; 12] = [
    "lamella_size",
    "lamella_bake",
    "lamella_run_on_device",
    "lamella_deploy",
    "lamella_debug_launch",
    "lamella_debug_set_breakpoints",
    "lamella_debug_continue",
    "lamella_debug_step",
    "lamella_debug_stack",
    "lamella_debug_locals",
    "lamella_debug_eval",
    "lamella_debug_disconnect",
];

/// Whether this build serves the tool `name` declares.
fn served(name: &str) -> bool {
    cfg!(feature = "bake") || !BAKE_ONLY.contains(&name)
}

/// The contract's entry for `name`, if this build serves it.
fn served_tool(name: &str) -> Option<&'static Value> {
    contract_tools()
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
        .filter(|_| served(name))
}

/// The tool-list payload: the contract's tools this build serves, in the contract's order.
///
/// **A LISTED TOOL IS ONE A CALL CAN REACH.** A build without `bake` listed all of them and then
/// refused every one it cannot run, so a client offered the model tools that only ever answered
/// that they were not built.
fn tools() -> Value {
    let served: Vec<Value> = contract_tools()
        .iter()
        .filter(|tool| tool.get("name").and_then(Value::as_str).is_some_and(served))
        .cloned()
        .collect();
    json!(served)
}

/// JSON-RPC's code for a line that is not JSON.
const PARSE_ERROR: i64 = -32700;
/// JSON-RPC's code for JSON that is not a request.
const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC's code for a method this server does not have.
const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC's code for a request whose parameters are wrong, which is also how the specification
/// has a server answer a call to a tool it does not have.
const INVALID_PARAMS: i64 = -32602;

/// Why a `tools/call` is answered without running its tool.
enum CallRefused {
    /// A malformed request, or a tool this server does not serve: a JSON-RPC error.
    Protocol(i64, String),
    /// Arguments the tool's `inputSchema` rules out: a tool result with `isError`, which is the
    /// answer a client hands the model so it can correct the call.
    Tool(Value),
}

/// The tool a `tools/call` names and the arguments to call it with, once both are checked.
///
/// **TWO KINDS OF WRONG, ANSWERED TWO WAYS, AS THE SPECIFICATION DIVIDES THEM.** A request that names
/// no tool, names one this server does not serve, or carries `arguments` that is not an object, is a
/// malformed request and gets a JSON-RPC error. Arguments that do not satisfy the tool's own
/// `inputSchema` are an input the caller can correct, and get a tool result with `isError`.
fn checked_call(params: Option<&Value>) -> Result<(&'static str, Value), CallRefused> {
    let Some(name) = params.and_then(|params| params.get("name")).and_then(Value::as_str) else {
        return Err(CallRefused::Protocol(
            INVALID_PARAMS,
            "tools/call needs the name of a tool, in params.name".to_owned(),
        ));
    };
    let Some(tool) = served_tool(name) else {
        let why = if BAKE_ONLY.contains(&name) {
            format!(
                "Unknown tool: {name}. This server was built without the `bake` feature, which it \
                 needs: `cargo build -p lamella-mcp --features bake` builds one with it."
            )
        } else {
            format!("Unknown tool: {name}")
        };
        return Err(CallRefused::Protocol(INVALID_PARAMS, why));
    };
    let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
    let arguments = match params.and_then(|params| params.get("arguments")) {
        None => json!({}),
        Some(object @ Value::Object(_)) => object.clone(),
        Some(_) => {
            return Err(CallRefused::Protocol(
                INVALID_PARAMS,
                format!("tools/call {name}: arguments must be an object"),
            ));
        }
    };
    if let Err(why) = schema::check(&arguments, &tool["inputSchema"]) {
        return Err(CallRefused::Tool(text_result(
            format!("{name} was not run: {why}. Its inputSchema says what it takes."),
            true,
        )));
    }
    Ok((name, arguments))
}

/// The resource metadata for `resources/list` (uri/name/description/mimeType; the text is served by read).
fn resources_list() -> Value {
    let items = serde_json::from_str::<Value>(RESOURCES)
        .ok()
        .and_then(|r| r.get("resources").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let meta: Vec<Value> = items
        .iter()
        .map(|r| json!({ "uri": r.get("uri"), "name": r.get("name"), "description": r.get("description"), "mimeType": r.get("mimeType") }))
        .collect();
    json!({ "resources": meta })
}

/// The `lamella://boards` body, COMPUTED from [`lamella_wire::product_model`] rather than stored.
///
/// That module is the one canonical value -> name map, so enumerating it here is the difference
/// between a list that is right and a list that was right when someone last copied it. It also
/// scales: a board is one row, and the boards a client actually cares about are the ones it can
/// see, which `lamella_list_devices` and `lamella_identify_device` report live.
///
/// The models are contiguous from 0, so the scan stops at the first unrecognized value -- the same
/// idiom `lamella-wire`'s `board-models-json` example uses to emit the JS registry.
fn boards_resource_text() -> String {
    let mut out = String::from(
        "# Supported dev boards\n\nThe `product_model` wire values a board reports over Lamella Link:\n\n\
         | model | board |\n|--:|---|\n",
    );
    let mut model: u16 = 0;
    while let Some(name) = lamella_wire::product_model::name(model) {
        out.push_str(&format!("| {model} | {name} |\n"));
        model += 1;
    }
    out.push_str(
        "\nRun `lamella_list_devices` to see the boards attached to this machine, and \
         `lamella_identify_device` on one of them to read its `product_model` and chip IDCODE live.\n",
    );
    out
}

/// The contents for `resources/read`, or `None` if the uri is unknown.
fn read_resource(uri: &str) -> Option<Value> {
    let parsed = serde_json::from_str::<Value>(RESOURCES).ok()?;
    let items = parsed.get("resources").and_then(Value::as_array)?;
    let r = items.iter().find(|r| r.get("uri").and_then(Value::as_str) == Some(uri))?;
    let text = if uri == "lamella://boards" {
        json!(boards_resource_text())
    } else {
        r.get("text").cloned().unwrap_or_else(|| json!(""))
    };
    Some(json!({ "contents": [{ "uri": r.get("uri"), "mimeType": r.get("mimeType"), "text": text }] }))
}

fn text_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// Parse a `usb` / `usb:<serial>` / `usb:<vid>:<pid>[:<serial>]` target into (vid, pid, serial).
fn parse_usb(target: &str) -> (u16, u16, Option<String>) {
    let rest = target.strip_prefix("usb").unwrap_or("").trim_start_matches(':');
    if rest.is_empty() {
        return (0, 0, None);
    }
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() >= 2 {
        if let (Ok(vid), Ok(pid)) = (u16::from_str_radix(parts[0], 16), u16::from_str_radix(parts[1], 16)) {
            return (vid, pid, parts.get(2).map(|s| (*s).to_owned()));
        }
    }
    (0, 0, Some(rest.to_owned()))
}

/// Open the carrier named by `target`: `usb[:...]` -> native-USB, anything else -> a serial port. The
/// `WireTransport` enum abstracts either so every device tool shares one open path.
fn open_transport(target: &str) -> Result<WireTransport, TransportError> {
    if target == "usb" || target.starts_with("usb:") {
        let (vid, pid, serial) = parse_usb(target);
        Ok(WireTransport::Usb(UsbTransport::open_matching(vid, pid, serial.as_deref())?))
    } else {
        Ok(WireTransport::Serial(SerialTransport::open(target, BAUD)?))
    }
}

/// A generous host capability set to advertise on HELLO -- offering `PROFILE_CHIPID` so a target that fills its
/// chip identity sends it. The negotiated set is this intersected with the target's, so it reflects the board.
fn host_caps() -> Capabilities {
    Capabilities(
        Capabilities::PROFILE_CHIPID
            | Capabilities::DEBUG_BASIC
            | Capabilities::BREAKPOINTS
            | Capabilities::STEPPING
            | Capabilities::REPL_RUN
            | Capabilities::BAKED_IMAGE
            | Capabilities::DEBUG_BOOT_DEPLOYED
            | Capabilities::DEPLOY_PREFIX_CRC,
    )
}

/// A display name for a Lamella Link `product_model`. DERIVES from [`lamella_wire::product_model::name`] -- the ONE
/// canonical value -> name map -- so it cannot drift from the registry. (Hand-mirroring it drifted twice: "SAM E54"
/// for canonical "SAME54", and four boards missing entirely.) `None` means the registry does not know the code;
/// UNKNOWN (0) names ITSELF "custom board", which is a recognized answer rather than an unrecognized one.
fn product_model_name(model: u16) -> Option<&'static str> {
    lamella_wire::product_model::name(model)
}

/// Enumerate attached boards (native-USB Lamella Link devices + OS serial ports), cross-platform.
/// `lamella_boards`: every board this build knows. The peer of the CLI's `boards` verb, reading the
/// SAME compiled-in catalog -- not a second list.
///
/// **AND THE SAME COLUMNS**, which is the half that is easy to lose: two listings can agree on
/// every row and still answer different questions. `can_flash` is here because "which boards exist"
/// and "which boards can I write" are the second question a caller asks, and an assistant that
/// cannot see the answer discovers it by attempting a write.
/// `lamella_version`: what this build is, and what it will accept.
///
/// **THE POINT IS THAT AN ASSISTANT ANSWERS "WHICH VERSION" FROM DATA RATHER THAN FROM MEMORY.** A
/// model asked what a toolchain supports will otherwise answer from training, which is a claim
/// about some build and not about this one -- and the numbers that decide interoperation are not
/// the one a release note names.
///
fn tool_version() -> Value {
    let it = lamella_flash_routes::contracts::Contracts::of(env!("CARGO_PKG_VERSION"));
    text_result(
        serde_json::to_string_pretty(&json!({
            "tool": it.tool,
            "link_protocol": it.wire_protocol,
            "sidecar_schema": it.sidecar_schema,
            "boards": it.boards,
            "flashable": it.flashable,
            "reads_as": it.describe(),
        }))
        .unwrap_or_default(),
        false,
    )
}

fn tool_boards() -> Value {
    let mut items: Vec<Value> = Vec::new();
    for (id, _) in lamella_catalog::BOARDS {
        match lamella_catalog::resolve(id) {
            Ok((board, part)) => items.push(json!({
                "board": id,
                "part": part.part,
                "flash_bytes": part.flash,
                "ram_bytes": part.ram,
                "family": board.family,
                "can_flash": lamella_flash_routes::can_flash(id),
            })),
            Err(why) => items.push(json!({ "board": id, "unresolved": why })),
        }
    }
    text_result(serde_json::to_string_pretty(&json!(items)).unwrap_or_default(), false)
}

/// `lamella_fit`: does an image of `image_bytes` fit on `board`?
///
/// **THE VERDICT IS `lamella_bsp_gen::fit::fit`, THE SAME FUNCTION THE CLI CALLS.** A budget that
/// meant one thing to a developer and another to an assistant would be the drift this fact stratum
/// exists to prevent, and an assistant is the consumer least able to notice it.
fn tool_fit(board: &str, image_bytes: Option<i64>) -> Value {
    let Some(image_bytes) = image_bytes else {
        return text_result("lamella_fit: image_bytes is required".to_owned(), true);
    };
    let (board_table, part) = match lamella_catalog::resolve(board) {
        Ok(resolved) => resolved,
        Err(why) => return text_result(format!("lamella_fit: {why}"), true),
    };
    let verdict = lamella_bsp_gen::fit::fit(&board_table, &part, image_bytes);
    text_result(serde_json::to_string_pretty(&fit_json(&verdict)).unwrap_or_default(), false)
}

/// A fit verdict as JSON, rendered rather than tabulated because the consumer is a program.
///
/// **`assumptions` AND `not_answered` ARE CARRIED, NOT DROPPED.** The verdict type's own doc says a
/// verdict without them is the failure this design exists to avoid -- somebody plans a product
/// around "it fits in the 8 MB PSRAM", the board arrives BARE, and it does not fit. A tool that
/// returned only a boolean would reintroduce exactly that, to the reader least able to know what
/// was taken as given.
fn fit_json(verdict: &lamella_bsp_gen::fit::FitVerdict) -> Value {
    json!({
        "board": verdict.board,
        "part": verdict.part,
        "image_bytes": verdict.image_bytes,
        "flash": { "bytes": verdict.flash.bytes, "source": format!("{:?}", verdict.flash.source) },
        "flash_fit": format!("{:?}", verdict.flash_fit),
        "ram": { "bytes": verdict.ram.bytes, "source": format!("{:?}", verdict.ram.source) },
        "assumptions": verdict.assumptions,
        "not_answered": verdict.not_answered,
    })
}

/// `lamella_reconcile`: is the attached board the one assumed?
fn tool_reconcile(board: &str, readings: Option<&Value>) -> Value {
    let (board_table, part) = match lamella_catalog::resolve(board) {
        Ok(resolved) => resolved,
        Err(why) => return text_result(format!("lamella_reconcile: {why}"), true),
    };
    let mut observed: Vec<lamella_bsp_gen::reconcile::Observation> = Vec::new();
    if let Some(Value::Object(map)) = readings {
        for (name, value) in map {
            let Some(reading) = value.as_i64() else {
                return text_result(
                    format!("lamella_reconcile: reading {name:?} is not an integer"),
                    true,
                );
            };
            observed.push(lamella_bsp_gen::reconcile::Observation {
                discriminator: name.clone(),
                reading,
            });
        }
    }
    let verdict = lamella_bsp_gen::reconcile::reconcile(&board_table, &part, &[], &observed);
    let profile: Vec<Value> = verdict
        .profile
        .iter()
        .map(|report| {
            json!({
                "claim": format!("{:?}", report.claim),
                "status": format!("{:?}", report.status),
            })
        })
        .collect();
    text_result(
        serde_json::to_string_pretty(&json!({
            "board": verdict.board,
            "part": verdict.part,
            "outcome": format!("{:?}", verdict.outcome),
            "profile": profile,
        }))
        .unwrap_or_default(),
        false,
    )
}

fn tool_list_devices() -> Value {
    let mut items: Vec<Value> = Vec::new();
    if let Ok(boards) = UsbTransport::list() {
        for b in boards {
            let target = match &b.serial_number {
                Some(s) => format!("usb:{:04x}:{:04x}:{s}", b.vendor_id, b.product_id),
                None => format!("usb:{:04x}:{:04x}", b.vendor_id, b.product_id),
            };
            items.push(json!({ "carrier": "usb", "target": target, "vid": b.vendor_id, "pid": b.product_id, "serial": b.serial_number, "product": b.product }));
        }
    }
    for p in list_serial() {
        items.push(json!({ "carrier": "serial", "target": p.port, "vid": p.vid, "pid": p.pid, "serial": p.serial_number, "product": p.product }));
    }
    let text = if items.is_empty() {
        "(no boards found)".to_owned()
    } else {
        serde_json::to_string_pretty(&json!(items)).unwrap_or_default()
    };
    text_result(text, false)
}

/// Render the identity a HELLO negotiated: Lamella Link version, capabilities, the board, its target
/// ABI, which firmware build is answering, the chip's own identity, and one line per resident runtime.
fn format_identity(target: &str, neg: &Negotiated) -> String {
    let mut s = format!("target: {target}\nLamella Link version: {}\ncapabilities: {:#010x}\n", neg.version, neg.caps.0);
    let identity = &neg.identity;
    let board = product_model_name(identity.product_model).unwrap_or("(unrecognized product_model)");
    s.push_str(&format!("board: {board} (product_model {})\n", identity.product_model));
    match lamella_wire::arch::name(identity.arch) {
        Some(name) => s.push_str(&format!("arch: {name}\n")),
        None => s.push_str("arch: (not reported by this firmware)\n"),
    }
    if identity.firmware_version != [0, 0] {
        s.push_str(&format!(
            "firmware build: day {} build {}\n",
            identity.firmware_version[0], identity.firmware_version[1]
        ));
    }
    if identity.chip_id_kind == lamella_wire::chip_id_kind::NONE {
        s.push_str("chip id: (not reported by this firmware)\n");
    } else {
        s.push_str(&format!("chip id (kind {}): ", identity.chip_id_kind));
        for byte in &identity.chip_id {
            s.push_str(&format!("{byte:02x}"));
        }
        s.push('\n');
    }
    if identity.surfaces.is_empty() {
        s.push_str("resident runtimes: (none reported)\n");
    }
    for surface in &identity.surfaces {
        let version = surface.lib_version;
        s.push_str(&format!(
            "resident runtime: tier {} abi {} hash {:#018x} library {}.{}.{}.{}\n",
            surface.tier, surface.abi, surface.hash, version[0], version[1], version[2], version[3]
        ));
    }
    if let Some((tier, field)) = neg.unreadable_surface_version() {
        s.push_str(&format!(
            "WARNING: tier {tier} claims a resident library and reports no {field}. The firmware \
             could not read it, so this board cannot be version-compared against anything.\n"
        ));
    }
    s
}

/// HELLO a target and report what it is -- no deploy, no run.
fn tool_identify_device(target: &str) -> Value {
    if target.is_empty() {
        return text_result("identify: a 'target' (COM port or usb[:...]) is required.".to_owned(), true);
    }
    let mut transport = match open_transport(target) {
        Ok(t) => t,
        Err(e) => return text_result(format!("cannot open {target}: {e:?}"), true),
    };
    match hello_blocking(&mut transport, 1, host_caps(), Duration::from_secs(3)) {
        Ok(neg) => text_result(format_identity(target, &neg), false),
        Err(lamella_wire::TransportError::VersionMismatch { target_min, target_max }) => text_result(
            format!("{target}: {}", lamella_wire_host::version_mismatch(lamella_wire::PROTOCOL_VERSION, target_min, target_max)),
            true,
        ),
        Err(e) => text_result(format!("no HELLO_ACK from {target} (is serve firmware running?): {e:?}"), true),
    }
}

/// Compile a C# submission and BAKE its PE into a `.lmli` flash image (single-assembly: BCL references resolve
/// to the interpreter's intrinsics, the same shape `BakedSerialLink` deploys). One leaked buffer per bake --
/// the accepted host `Assembly<'static>` pattern. `write_baked` needs `code-in-place`, hence `bake`-gated.
#[cfg(feature = "bake")]
fn compile_and_bake(compiler: &LcscCompiler, code: &str) -> Result<Vec<u8>, String> {
    let pe = compiler.compile(code).map_err(|e| match e {
        CompileFailure::Diagnostics(d) => format!("compile failed:\n{d}"),
        CompileFailure::Toolchain(t) => format!("toolchain error: {t}"),
    })?;
    let program: &'static [u8] = Box::leak(pe.into_boxed_slice());
    let assembly = lamella_metadata::Assembly::read(program).map_err(|e| format!("PE does not parse: {e:?}"))?;
    let loaded = lamella_load::load(&assembly).map_err(|e| format!("load: {e}"))?;
    let mut module = loaded.module;
    module.write_baked(Some(loaded.entry)).map_err(|e| format!("bake: {e:?}"))
}

/// Compile a submission and run it ON the device over Lamella Link (BakedSerialLink: bake host-side, the image
/// crosses the wire, the device interprets it and returns output). Serial targets only.
#[cfg(feature = "bake")]
fn tool_run_on_device(scope: &DeviceScope, target: &str, code: &str) -> Value {
    if target.starts_with("usb") {
        return text_result("run_on_device supports serial targets today (BakedSerialLink); usb Lamella Link run is a follow-up.".to_owned(), true);
    }
    let compiler = match LcscCompiler::discover() {
        Ok(c) => c,
        Err(e) => return text_result(format!("compiler: {e}"), true),
    };
    let link = match BakedSerialLink::open(target, BAUD, Duration::from_secs(15)) {
        Ok(l) => l,
        Err(e) => return text_result(format!("cannot open {target} as a baked-image target (needs serve + BAKED_IMAGE): {e:?}"), true),
    };
    if let Err(why) = scope.permits_link_board(link.identity().product_model) {
        return text_result(why, true);
    }
    let mut repl = Repl::new(Box::new(compiler), Box::new(link));
    match repl.eval_program(code) {
        Ok(Outcome::Ran { output, exit, .. }) => {
            let body = if output.is_empty() { "(empty)".to_owned() } else { output };
            text_result(format!("[ran on {target}]\nexit code: {exit}\nstdout:\n{body}"), false)
        }
        Ok(Outcome::CompileError(t)) => text_result(format!("Compile failed:\n{t}"), true),
        Ok(Outcome::Empty) => text_result("(empty submission)".to_owned(), false),
        Err(e) => text_result(format!("device error: {e:?}"), true),
    }
}

/// Query the resident deployed image on a target (read-only): HELLO to enter serve mode, then DEPLOY_STATUS.
fn tool_deploy_status(target: &str) -> Value {
    let mut t = match open_transport(target) {
        Ok(t) => t,
        Err(e) => return text_result(format!("cannot open {target}: {e:?}"), true),
    };
    let timeout = Duration::from_secs(5);
    if let Err(e) = hello_blocking(&mut t, 0, host_caps(), timeout) {
        return text_result(format!("no HELLO_ACK from {target}: {e:?}"), true);
    }
    match deployed_status_blocking(&mut t, 1, timeout) {
        Ok(Some(sum)) => text_result(format!("{target} holds a resident image, checksum {sum:#018x}"), false),
        Ok(None) => text_result(format!("{target} has no resident image."), false),
        Err(e) => text_result(format!("status query on {target} failed: {e:?}"), true),
    }
}

/// Every tool that touches a board. Each is refused when the server was started without
/// `--allow-device`.
const DEVICE_TOOLS: [&str; 14] = [
    "lamella_list_devices",
    "lamella_identify_device",
    "lamella_run_on_device",
    "lamella_deploy",
    "lamella_deploy_status",
    "lamella_flash",
    "lamella_debug_launch",
    "lamella_debug_set_breakpoints",
    "lamella_debug_continue",
    "lamella_debug_step",
    "lamella_debug_stack",
    "lamella_debug_locals",
    "lamella_debug_eval",
    "lamella_debug_disconnect",
];

/// The tools that write a board over the Lamella Link: each changes what the board runs, or runs
/// code on it.
///
/// `--allow-device`'s scope binds each of them twice: before anything is opened
/// ([`DeviceScope::permits_link_request`]), and after the board answers its HELLO and before anything
/// is written ([`DeviceScope::permits_link_board`]). The debug tools that take a `session` act on a
/// board `lamella_debug_launch` already checked.
const LINK_WRITES: [&str; 3] = ["lamella_deploy", "lamella_run_on_device", "lamella_debug_launch"];

/// The refusal `lamella_flash` gives an `image` that is not a chip image, or `None` when it is one.
///
/// Decided by the file's KIND, before any file is read, as `lamella flash` decides it. Source or a
/// `.lmli` would otherwise reach the image reader and be called "not a format this can write", which
/// names no way forward.
fn refusal_unless_a_chip_image(image: &str) -> Option<Value> {
    use lamella_flash_routes::artifact::{Kind, classify};
    let why = match classify(std::path::Path::new(image)) {
        Kind::ChipImage => return None,
        Kind::Source => format!(
            "{image} is source, and lamella_flash writes an image that is already built. lamella_deploy \
             compiles a program and puts it on a board running Lamella; the `lamella build` verb \
             builds a chip image."
        ),
        Kind::WirePayload => format!(
            "{image} is loaded by a board that is already running Lamella, not written to a chip, so \
             lamella_flash cannot write it. `lamella deploy {image} --target <t>` sends it to such a \
             board, and lamella_deploy compiles a program from its source and sends that."
        ),
    };
    Some(text_result(why, true))
}

/// The refusal a tool call gets before it is dispatched, or `None`: a device tool when the server was
/// started without `--allow-device`, and a Lamella Link write whose scope cannot hold for it.
fn refusal_before_dispatch(scope: &DeviceScope, name: &str) -> Option<Value> {
    if DEVICE_TOOLS.contains(&name) && !scope.allowed {
        return Some(text_result(
            "This tool touches a hardware board; the server was started without --allow-device.".to_owned(),
            true,
        ));
    }
    if cfg!(feature = "bake")
        && LINK_WRITES.contains(&name)
        && let Err(why) = scope.permits_link_request()
    {
        return Some(text_result(why, true));
    }
    None
}

/// Server state: the host compile/run engine (+ a check-only compiler), and (added with the debug tools) the
/// live debug sessions. `allow_device` gates every tool that touches a board.
struct Server {
    check: LcscCompiler,
    /// How long a `lamella_run` program may run before its process is stopped ([`run_in_child`]).
    run_timeout: Duration,
    scope: DeviceScope,
    /// The one directory a tool may create a file in, from `--allow-write`. `None`: no tool writes a
    /// file.
    #[cfg_attr(not(feature = "bake"), allow(dead_code))]
    write_dir: Option<std::path::PathBuf>,
    #[cfg(feature = "bake")]
    debug: debug::Sessions,
}

impl Server {
    fn new(
        scope: DeviceScope,
        write_dir: Option<std::path::PathBuf>,
        run_timeout: Duration,
    ) -> Result<Self, String> {
        corlib_bytes()?;
        let check = LcscCompiler::discover().map_err(|e| e.to_string())?;
        Ok(Self {
            check,
            run_timeout,
            scope,
            write_dir,
            #[cfg(feature = "bake")]
            debug: debug::Sessions::new(),
        })
    }

    fn call_tool(&mut self, name: &str, args: &Value) -> Value {
        let lang = args.get("language").and_then(Value::as_str).unwrap_or("csharp");
        let code = args.get("code").and_then(Value::as_str).unwrap_or_default();
        let target = args.get("target").and_then(Value::as_str).unwrap_or_default();
        #[cfg(feature = "bake")]
        let session = args.get("session").and_then(Value::as_str).unwrap_or_default();
        #[cfg(feature = "bake")]
        let source = args.get("source").and_then(Value::as_str).unwrap_or_default();
        #[cfg(feature = "bake")]
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("over");
        #[cfg(feature = "bake")]
        let expression = args.get("expression").and_then(Value::as_str).unwrap_or_default();
        if let Some(refusal) = refusal_before_dispatch(&self.scope, name) {
            return refusal;
        }
        match name {
            "lamella_flash" => self.tool_flash(&args),
            "lamella_check" => self.tool_check(lang, code),
            "lamella_run" => self.tool_run(lang, code),
            "lamella_version" => tool_version(),
            "lamella_boards" => tool_boards(),
            "lamella_fit" => tool_fit(
                args.get("board").and_then(Value::as_str).unwrap_or_default(),
                args.get("image_bytes").and_then(Value::as_i64),
            ),
            "lamella_reconcile" => tool_reconcile(
                args.get("board").and_then(Value::as_str).unwrap_or_default(),
                args.get("readings"),
            ),
            "lamella_list_devices" => tool_list_devices(),
            "lamella_identify_device" => tool_identify_device(target),
            "lamella_deploy_status" => tool_deploy_status(target),
            #[cfg(feature = "bake")]
            "lamella_size" => self.tool_size(code),
            #[cfg(feature = "bake")]
            "lamella_bake" => self.tool_bake(code, args),
            #[cfg(feature = "bake")]
            "lamella_run_on_device" => tool_run_on_device(&self.scope, target, code),
            #[cfg(feature = "bake")]
            "lamella_deploy" => self.tool_deploy(target, code, args.get("run").and_then(Value::as_bool).unwrap_or(true)),
            #[cfg(feature = "bake")]
            "lamella_debug_launch" => self.debug.launch(&self.scope, target, code),
            #[cfg(feature = "bake")]
            "lamella_debug_set_breakpoints" => {
                let lines: Vec<i64> = args
                    .get("lines")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_i64).collect())
                    .unwrap_or_default();
                self.debug.set_breakpoints(session, &lines, source)
            }
            #[cfg(feature = "bake")]
            "lamella_debug_continue" => self.debug.cont(session),
            #[cfg(feature = "bake")]
            "lamella_debug_step" => self.debug.step(session, kind),
            #[cfg(feature = "bake")]
            "lamella_debug_stack" => self.debug.stack(session),
            #[cfg(feature = "bake")]
            "lamella_debug_locals" => self.debug.locals(session, args.get("frame").and_then(Value::as_i64).unwrap_or(0)),
            #[cfg(feature = "bake")]
            "lamella_debug_eval" => self.debug.eval(session, expression),
            #[cfg(feature = "bake")]
            "lamella_debug_disconnect" => self.debug.disconnect(session),
            other => text_result(format!("unknown tool: {other}"), true),
        }
    }

    /// `lamella_flash`: write a built image to a board's chip.
    ///
    /// **THE SAME ROUTES, THE SAME REFUSALS, AND THE SAME CONTRACT THE CLI USES.** Nothing about
    /// which board takes which mechanism is restated here; this resolves the request, applies the
    /// server's scope, and hands the write to `lamella_flash_routes`.
    ///
    /// It refuses rather than choosing at every point the CLI does, and the reason is sharper for
    /// an agent than for a person: a caller that cannot see the hardware has no way to notice that the
    /// board it wrote was not the board it meant.
    fn tool_flash(&self, args: &Value) -> Value {
        let image = args.get("image").and_then(Value::as_str).unwrap_or_default();
        let board = args.get("board").and_then(Value::as_str).unwrap_or_default();
        let via = args.get("via").and_then(Value::as_str);
        let probe = args.get("probe").and_then(Value::as_str);
        let volume = args.get("volume").and_then(Value::as_str);
        let device = args.get("device").and_then(Value::as_str);

        if image.is_empty() || board.is_empty() {
            return text_result(
                "lamella_flash needs both `image` (a path to a BUILT image) and `board`.".to_owned(),
                true,
            );
        }

        if !self.scope.permits_request(board, probe) {
            return text_result(
                format!(
                    "This server may not write {board}{}. It was started with: {}.\n\n\
That is set when the server starts.",
                    probe.map_or(String::new(), |p| format!(" through probe {p}")),
                    self.scope.describe()
                ),
                true,
            );
        }

        if let Some(refusal) = refusal_unless_a_chip_image(image) {
            return refusal;
        }

        let path = std::path::Path::new(image);
        let manifest = match lamella_flash_routes::manifest::read(path) {
            Ok(manifest) => manifest,
            Err(why) => {
                return text_result(
                    format!(
                        "{why}\n\nA sidecar that is present and cannot be read is a claim about \
                         this image that nobody can check, so nothing was written."
                    ),
                    true,
                );
            }
        };
        if let Some(manifest) = manifest.as_ref() {
            if let Err(why) = lamella_flash_routes::manifest::check_board(manifest, board) {
                return text_result(why, true);
            }
            let shipped = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) => return text_result(format!("read {image}: {error}"), true),
            };
            let extension = lamella_flash_routes::artifact::classify_format(path);
            if let Err(why) = lamella_flash_routes::manifest::check_identity(
                manifest,
                &shipped,
                extension.as_deref(),
            )
            {
                return text_result(why, true);
            }
        }

        let row = match lamella_flash_routes::programmer_for(board) {
            Ok(row) => row,
            Err(error) => return text_result(error, true),
        };
        let chosen = match lamella_flash_routes::route_for(row, via) {
            Ok(programmer) => programmer,
            Err(error) => return text_result(error, true),
        };
        let selector = match lamella_flash_routes::selector_for(chosen, probe, volume, device) {
            Ok(selector) => selector,
            Err(error) => return text_result(error, true),
        };

        let placement = match lamella_flash_routes::placement::placement_for(
            row,
            chosen,
            lamella_flash_routes::placement::BootloaderChoice::Keep,
        ) {
            Ok(placement) => placement,
            Err(error) => return text_result(error, true),
        };
        let prepared = match lamella_flash_routes::prepare_image(path, row, chosen, &placement) {
            Ok(prepared) => prepared,
            Err(error) => return text_result(error, true),
        };

        match lamella_flash_routes::write_scoped(
            chosen,
            &placement,
            &prepared.bytes,
            selector.as_deref(),
            &self.scope.identities(),
        ) {
            Ok(report) => text_result(
                format!(
                    "Wrote {} B to {board} over {}.\n\
The part answered {:#x} -- {}.\n\
Verification: {}.",
                    report.bytes,
                    report.mechanism,
                    report.identity.value,
                    report.identity.what,
                    match report.verification {
                        lamella_flash_backend::Verification::ReadBack =>
                            "every byte was read back and compared".to_owned(),
                        lamella_flash_backend::Verification::NotPossible(what) =>
                            format!("NONE -- {what} cannot read the flash back"),
                        lamella_flash_backend::Verification::Skipped =>
                            "SKIPPED at the caller's request".to_owned(),
                    }
                ),
                false,
            ),
            Err(error) => text_result(error, true),
        }
    }

    fn tool_check(&self, lang: &str, code: &str) -> Value {
        if lang == "python" {
            return text_result("Python check runs on the browser host; not wired into the native server yet.".to_owned(), false);
        }
        match self.check.compile(code) {
            Ok(_) => text_result("OK -- compiles.".to_owned(), false),
            Err(CompileFailure::Diagnostics(text)) => text_result(text, false),
            Err(CompileFailure::Toolchain(text)) => text_result(format!("toolchain error: {text}"), true),
        }
    }

    fn tool_run(&self, lang: &str, code: &str) -> Value {
        if lang == "python" {
            return text_result("Python run is on the browser host; not wired into the native server yet.".to_owned(), false);
        }
        run_in_child(code, self.run_timeout)
    }

    #[cfg(feature = "bake")]
    fn tool_bake(&self, code: &str, args: &Value) -> Value {
        if args.get("out_path").is_some() {
            return text_result(
                "lamella_bake takes no out_path. Name a new file with out_name: it is written in the \
                 directory the server was started with --allow-write=<dir>."
                    .to_owned(),
                true,
            );
        }
        let out_name = args.get("out_name").and_then(Value::as_str);
        if let Some(name) = out_name
            && let Err(why) = writable(self.write_dir.as_deref(), name)
        {
            return text_result(format!("Nothing was baked or written: {why}."), true);
        }
        match compile_and_bake(&self.check, code) {
            Ok(image) => {
                let mut msg = format!("baked OK: {} bytes (.lmli flash image).", image.len());
                if let Some(name) = out_name {
                    match write_new_file(self.write_dir.as_deref(), name, &image) {
                        Ok(path) => msg.push_str(&format!("\nWrote {}, a new file.", path.display())),
                        Err(why) => return text_result(format!("{msg}\nNothing was written: {why}."), true),
                    }
                }
                text_result(msg, false)
            }
            Err(e) => text_result(e, true),
        }
    }

    #[cfg(feature = "bake")]
    fn tool_size(&self, code: &str) -> Value {
        match compile_and_bake(&self.check, code) {
            Ok(image) => text_result(
                format!(
                    "flash (baked .lmli image): {} bytes\n(This is the deployable image size; it is not a RAM or per-section budget.)",
                    image.len()
                ),
                false,
            ),
            Err(e) => text_result(e, true),
        }
    }

    #[cfg(feature = "bake")]
    fn tool_deploy(&self, target: &str, code: &str, run: bool) -> Value {
        let image = match compile_and_bake(&self.check, code) {
            Ok(i) => i,
            Err(e) => return text_result(e, true),
        };
        let mut t = match open_transport(target) {
            Ok(t) => t,
            Err(e) => return text_result(format!("cannot open {target}: {e:?}"), true),
        };
        deploy_over(&self.scope, &mut t, target, &image, run, Duration::from_secs(20))
    }
}

/// The argument the server starts itself with to run one `lamella_run` program ([`run_child`]).
const RUN_CHILD: &str = "--run-child";

/// How long a `lamella_run` program may run when the server was started without `--run-timeout`.
const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(20);

/// The most objects a `lamella_run` program may hold live at once. Past it an allocation raises
/// `OutOfMemoryException`, which the program can catch, rather than taking this machine's memory.
const RUN_OBJECT_BUDGET: usize = 1_000_000;

/// The most console output, in UTF-16 units, a `lamella_run` program's run keeps.
const RUN_OUTPUT_CAP: usize = 1_000_000;

/// How much longer than its deadline the process a run is done in lets itself live: its own stop,
/// for when the server that started it is gone ([`run_child`]).
const RUN_CHILD_GRACE: Duration = Duration::from_secs(5);

/// How long `--run-timeout=<seconds>` lets a `lamella_run` program run, or [`DEFAULT_RUN_TIMEOUT`].
fn parse_run_timeout(args: impl Iterator<Item = String>) -> Result<Duration, String> {
    let mut timeout = None;
    for arg in args {
        let Some(rest) = arg.strip_prefix("--run-timeout") else { continue };
        let seconds = rest
            .strip_prefix('=')
            .and_then(|seconds| seconds.parse::<u64>().ok())
            .filter(|seconds| (1..=3600).contains(seconds))
            .ok_or_else(|| format!("{arg}: give whole seconds from 1 to 3600, as --run-timeout=<seconds>"))?;
        if timeout.replace(Duration::from_secs(seconds)).is_some() {
            return Err("--run-timeout was given more than once".to_owned());
        }
    }
    Ok(timeout.unwrap_or(DEFAULT_RUN_TIMEOUT))
}

/// Run `code` for `lamella_run` in a process of its own, and stop that process at `timeout`.
///
/// **THE ONLY STOP THAT ALWAYS WORKS IS ONE OUTSIDE THE PROGRAM.** The interpreter has no step
/// budget, and a thread cannot be stopped from outside, so a program that never ended held the
/// server's one request loop for good. In a child process it holds only that process, which is
/// stopped at the deadline, and the server answers the next request. A program that crashes the
/// interpreter, or fills memory, takes only the child with it.
fn run_in_child(code: &str, timeout: Duration) -> Value {
    let started = std::env::current_exe().and_then(|exe| {
        std::process::Command::new(exe)
            .arg(RUN_CHILD)
            .arg(timeout.as_millis().to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
    });
    let mut child = match started {
        Ok(child) => child,
        Err(error) => {
            return text_result(format!("error: could not start a process to run the program in: {error}"), true);
        }
    };
    let source = code.as_bytes().to_vec();
    let feeding = child.stdin.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let _ = pipe.write_all(&source);
        })
    });
    let reading = child.stdout.take().map(drain);
    let erring = child.stderr.take().map(drain);

    let deadline = Instant::now() + timeout;
    let finished = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    if let Some(feeding) = feeding {
        let _ = feeding.join();
    }
    let answer = reading.and_then(|reading| reading.join().ok()).unwrap_or_default();
    let complaint = erring.and_then(|erring| erring.join().ok()).unwrap_or_default();
    if !finished {
        return text_result(
            format!(
                "The program was stopped after {} seconds without finishing. One that loops forever, or \
                 waits for something this machine does not have, never finishes here.",
                timeout.as_secs()
            ),
            true,
        );
    }
    let answer = String::from_utf8_lossy(&answer)
        .lines()
        .last()
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .unwrap_or_default();
    let text = answer["text"].as_str().unwrap_or_default();
    match answer["outcome"].as_str() {
        Some("ran") => {
            let output = answer["output"].as_str().unwrap_or_default();
            let body = if output.is_empty() { "(empty)" } else { output };
            text_result(format!("exit code: {}\nstdout:\n{body}", answer["exit"]), false)
        }
        Some("compile_error") => text_result(format!("Compile failed:\n{text}"), true),
        Some("empty") => text_result("(empty submission)".to_owned(), false),
        Some("error") => text_result(format!("error: {text}"), true),
        _ => {
            let said = String::from_utf8_lossy(&complaint);
            let said = said.trim();
            let tail = said.char_indices().rev().nth(1999).map_or(said, |(at, _)| &said[at..]);
            text_result(format!("error: the run ended without an answer. It said:\n{tail}"), true)
        }
    }
}

/// Read a child's pipe to its end, on a thread of its own.
fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

/// `lamella-mcp --run-child <milliseconds>`: read one program from stdin, run it on this machine's
/// interpreter, and print one JSON line saying how it went. The server starts itself this way for
/// every `lamella_run` ([`run_in_child`]), passing the run's deadline.
///
/// **IT CARRIES A STOP OF ITS OWN.** The server stops it at the deadline; if the server is gone
/// first, nothing else would stop a program that never ends, so this process ends itself
/// [`RUN_CHILD_GRACE`] after the deadline, whatever the program is doing.
fn run_child() -> ! {
    let deadline = std::env::args()
        .nth(2)
        .and_then(|milliseconds| milliseconds.parse::<u64>().ok())
        .map_or(DEFAULT_RUN_TIMEOUT, Duration::from_millis);
    std::thread::spawn(move || {
        std::thread::sleep(deadline + RUN_CHILD_GRACE);
        std::process::exit(3);
    });
    let mut source = String::new();
    let answer = match std::io::stdin().read_to_string(&mut source) {
        Err(error) => json!({ "outcome": "error", "text": format!("the program could not be read: {error}") }),
        Ok(_) => match child_repl() {
            Err(error) => json!({ "outcome": "error", "text": error }),
            Ok(mut repl) => match repl.eval_program(&source) {
                Ok(Outcome::Ran { output, exit, .. }) => json!({ "outcome": "ran", "exit": exit, "output": output }),
                Ok(Outcome::CompileError(text)) => json!({ "outcome": "compile_error", "text": text }),
                Ok(Outcome::Empty) => json!({ "outcome": "empty" }),
                Err(error) => json!({ "outcome": "error", "text": error.to_string() }),
            },
        },
    };
    println!("{answer}");
    let _ = std::io::stdout().flush();
    std::process::exit(0)
}

/// The engine a `lamella_run` program runs on: this build's compiler, and the interpreter over the
/// corlib, set up by [`configure_run`].
fn child_repl() -> Result<Repl, String> {
    let corlib = corlib_bytes()?;
    let compiler = LcscCompiler::discover().map_err(|error| error.to_string())?;
    Ok(Repl::new(Box::new(compiler), Box::new(LoopbackLink::new(corlib, configure_run))))
}

/// The machine a `lamella_run` program runs on: this machine's clock, and bounds on what the
/// program may hold.
fn configure_run(vm: &mut lamella_cil_runtime::Vm) {
    install_host_clock(vm);
    vm.set_object_budget(Some(RUN_OBJECT_BUDGET));
    vm.set_output_cap(Some(RUN_OUTPUT_CAP));
}

/// `lamella_deploy` on an open carrier: HELLO the board, refuse it unless the scope names it, then
/// deploy `image` and, if `run`, start it.
#[cfg(feature = "bake")]
fn deploy_over(
    scope: &DeviceScope,
    t: &mut impl lamella_wire::Transport,
    target: &str,
    image: &[u8],
    run: bool,
    timeout: Duration,
) -> Value {
    let session = match hello_blocking(&mut *t, 0, host_caps(), timeout) {
        Ok(session) => session,
        Err(e) => return text_result(format!("no HELLO_ACK from {target}: {e:?}"), true),
    };
    if let Err(why) = scope.permits_link_board(session.identity.product_model) {
        return text_result(why, true);
    }
    match deploy_image_blocking(&mut *t, 1, image, 8 * 1024, timeout, session.caps) {
        Ok(TransferAck::Accepted) => {
            let mut msg = format!("deployed {} bytes to {target}.", image.len());
            let mut failed = false;
            if run {
                let (said, is_error) = start_deployed(&mut *t, START_ACK_PATIENCE);
                msg.push_str(&said);
                failed = is_error;
            }
            text_result(msg, failed)
        }
        Ok(not_accepted) => text_result(
            format!("deploy to {target} failed: {}.", not_accepted.describe(board_name(session.identity.product_model))),
            true,
        ),
        Err(e) => text_result(format!("deploy to {target} failed: {e:?}"), true),
    }
}


/// How long `lamella_deploy` waits for the board to acknowledge a start. It answers before the reset
/// the start implies, so the answer is prompt.
#[cfg(feature = "bake")]
const START_ACK_PATIENCE: Duration = Duration::from_secs(2);

/// What `lamella_deploy` adds to its answer after asking the board to start the image it deployed, and
/// whether that answer is an error.
///
/// **ONLY THE BOARD's ACKNOWLEDGEMENT MAKES IT A START.** A start the board refuses, or never answers,
/// makes the answer an error that still says the image is on the board, so a caller that cannot see the
/// board is not told a program is running when none is.
#[cfg_attr(not(feature = "bake"), allow(dead_code))]
fn start_deployed(transport: &mut impl lamella_wire::Transport, patience: Duration) -> (String, bool) {
    use lamella_wire_host::{StartFailure, exec};
    match lamella_wire_host::start_execution(transport, 2, exec::exec_source::DEPLOYED, 0, patience) {
        Ok(()) => (" Started it: the board acknowledged the start.".to_owned(), false),
        Err(StartFailure::NoAnswer) => (
            " The board did not acknowledge the start, so whether it is running is not known; resetting \
             the board runs it."
                .to_owned(),
            true,
        ),
        Err(failure) => (format!(" But {failure}."), true),
    }
}

/// Add the caching hints the specification requires on a `resultType: "complete"` result.
///
/// **REQUIRED, NOT ADVISORY**, on `server/discover`, `tools/list`, `resources/list` and
/// `resources/read`. A client that receives none assumes the result is immediately stale, so
/// omitting them does not fail loudly -- it just makes every client re-fetch a list that cannot
/// have changed.
///
/// `public` because nothing here varies per caller: the tools and resources are compiled in, and
/// the same for whoever asks.
fn with_cache_hints(mut result: Value) -> Value {
    if let Some(object) = result.as_object_mut() {
        object.insert("resultType".to_owned(), json!("complete"));
        object.insert("ttlMs".to_owned(), json!(CACHE_TTL_MS));
        object.insert("cacheScope".to_owned(), json!("public"));
    }
    result
}

/// The revision a request declares, from the modern `_meta` field.
///
/// A request without one is a legacy request; this returns `None` and the caller serves it under
/// the legacy path rather than rejecting it.
fn requested_protocol(msg: &Value) -> Option<&str> {
    msg.get("params")?.get("_meta")?.get(META_PROTOCOL_VERSION)?.as_str()
}

/// The next request or notification on `input`, or `None` at its end.
///
/// **A LINE THAT IS NOT A MESSAGE IS ANSWERED AND PASSED OVER, NEVER THE END OF THE SERVER.** A line
/// that is not UTF-8, or not JSON, is a parse error; JSON that is not a request object is an invalid
/// request. Each is answered with `id: null`, as JSON-RPC requires when the id cannot be read. A
/// response (an object with no method) needs no answer, since this server sends no requests.
fn next_message(input: &mut impl BufRead, out: &mut impl Write) -> Option<Value> {
    let mut line = Vec::new();
    loop {
        line.clear();
        match input.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            respond_error(out, &Value::Null, PARSE_ERROR, "Parse error: the line is not UTF-8");
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let message = match serde_json::from_str::<Value>(text) {
            Ok(message) => message,
            Err(error) => {
                respond_error(out, &Value::Null, PARSE_ERROR, &format!("Parse error: {error}"));
                continue;
            }
        };
        if message.get("method").is_some_and(Value::is_string) {
            return Some(message);
        }
        let response = message.get("result").is_some() || message.get("error").is_some();
        if !response {
            let id = message.get("id").filter(|id| id.is_string() || id.is_number()).cloned();
            respond_error(
                out,
                &id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "Invalid Request: a request is a JSON object with a method",
            );
        }
    }
}

fn respond(out: &mut impl Write, id: &Value, result: Value) {
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "result": result })).unwrap_or_default()
    );
    let _ = out.flush();
}

/// A JSON-RPC error carrying a `data` payload.
///
/// `UnsupportedProtocolVersionError` is only useful WITH its data: the `supported` list is what the
/// client retries against, and without it the client has been told "no" and nothing else.
fn respond_error_with_data(out: &mut impl Write, id: &Value, code: i64, message: &str, data: Value) {
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message, "data": data }
        }))
        .unwrap_or_default()
    );
    let _ = out.flush();
}

fn respond_error(out: &mut impl Write, id: &Value, code: i64, message: &str) {
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
            .unwrap_or_default()
    );
    let _ = out.flush();
}


/// What the server was permitted to touch, parsed from `--allow-device`.
///
/// **IT BINDS EVERY TOOL THAT WRITES A BOARD:** `lamella_flash`, which writes a chip through a probe
/// or a bootloader drive, and the [`LINK_WRITES`], which write a board over the Lamella Link. The
/// tools that list and identify boards need only `--allow-device`, because identifying a board is
/// how a caller finds the one in scope.
///
/// **A SCOPE ON A REQUEST AND A SCOPE ON A DEVICE ARE DIFFERENT PROTECTIONS**, and only one of them
/// survives a cable being moved:
///
/// - `board:` and `probe:` narrow what a flash may ASK FOR. They are checked here, before any
///   hardware is touched, and they are cheap.
/// - `chip:` narrows what a flash may WRITE. It is checked by the flashing contract itself, between
///   the part identifying itself and anything being erased, against the reading the part gave.
/// - A Link write can satisfy only `board:`, and there it is checked against the board itself: the
///   model it reports in its HELLO, on the connection the write then crosses
///   ([`permits_link_board`](Self::permits_link_board)). A `probe:` or `chip:` term refuses every
///   Link write ([`permits_link_request`](Self::permits_link_request)).
///
/// **`chip:` IS ONLY AS NARROW AS THE READING THE PART CAN GIVE, AND THAT VARIES BY PART.** An
/// RP2350 publishes a 64-bit OTP chip id unique to the die, so a `chip:` scope there really does
/// answer "may I erase THIS one". An STM32L0 answers `DEV_ID`, which names a product CATEGORY that
/// every part in it gives -- so on a bench of two NUCLEO-L073RZs a single `chip:` scope permits
/// both. A backend says which kind it handed over: `PartIdentity::what` reads "the category, which
/// every part in it answers, not this board" exactly where the value is not unique.
///
/// So the ranking is not fixed. On a part with a die-unique id, `chip:` is the only scope that
/// survives somebody moving a cable, because a probe serial names a cable and a board id names a
/// family. On a part that can only name its category, `chip:` is a third family check and the board
/// still needs `probe:`. The first two are worth having either way: they refuse in a millisecond
/// and without a probe, and a mistake they catch never reaches a board.
///
/// **WHAT THIS IS NOT IS AN ACCESS CONTROL, AND THAT IS WORTH STATING PLAINLY.** Whoever can start
/// this server can start it with a wider scope, and whoever can run this server can usually run the
/// `lamella` CLI instead, which asks nobody. The scope narrows what THIS surface will do; it does
/// not narrow what the process can reach.
///
/// That is not a defect to be fixed by hardening the tool. This surface exists to make Lamella easy
/// to discover and hard to use wrongly BY ACCIDENT -- the wrong board, the wrong image, a
/// verification nobody performed. A guard sold as a sandbox would be worse than none, because
/// somebody would rely on it.
#[derive(Debug, Clone, Default)]
struct DeviceScope {
    /// Whether any hardware tool may run at all.
    allowed: bool,
    /// Board ids this server may write. Empty = any.
    boards: Vec<String>,
    /// Probe serials this server may write through. Empty = any.
    probes: Vec<String>,
    /// Chip identities this server may write. Empty = any.
    chips: Vec<u64>,
}

impl DeviceScope {
    /// Parse every `--allow-device` argument.
    ///
    /// Bare `--allow-device` permits any known board -- the behavior before scopes existed, kept
    /// because a single-board bench does not need a narrowing and should not be made to write one.
    /// `--allow-device=<term>[,<term>]` narrows, and terms of the SAME kind are alternatives while
    /// terms of DIFFERENT kinds must all hold: `board:rpi-pico2,probe:E66` means that model through
    /// that probe.
    ///
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut scope = DeviceScope::default();
        for arg in args {
            let Some(rest) = arg.strip_prefix("--allow-device") else { continue };
            scope.allowed = true;
            let Some(terms) = rest.strip_prefix('=') else {
                if rest.is_empty() {
                    continue;
                }
                return Err(format!("unknown argument {arg}"));
            };
            for term in terms.split(',').map(str::trim).filter(|t| !t.is_empty()) {
                let (key, value) = term.split_once(':').ok_or_else(|| {
                    format!(
                        "--allow-device={term}: a scope needs a kind. Use board:<id>, probe:<serial> or\n\
chip:<hex>. Bare --allow-device permits any board this build can write."
                    )
                })?;
                match key {
                    "board" if lamella_catalog::load_board(value).is_none() => {
                        return Err(format!(
                            "--allow-device=board:{value}: this build knows no board {value:?}. \
                             `lamella boards` lists the ids it knows."
                        ));
                    }
                    "board" => scope.boards.push(value.to_owned()),
                    "probe" => scope.probes.push(value.to_owned()),
                    "chip" => {
                        let hex = value.trim_start_matches("0x");
                        let id = u64::from_str_radix(hex, 16).map_err(|_| {
                            format!(
                                "--allow-device=chip:{value}: a chip scope is the part's own id in hex. An RP2350\n\
publishes its 64-bit OTP chip id as its bootloader USB serial, and `lamella flash`\n\
prints it after every write."
                            )
                        })?;
                        scope.chips.push(id);
                    }
                    other => {
                        return Err(format!(
                            "--allow-device: {other} is not a scope kind. Use board:, probe: or chip:."
                        ));
                    }
                }
            }
        }
        Ok(scope)
    }

    /// Whether a request naming `board` through `probe` is inside the scope.
    ///
    /// Checked BEFORE any hardware is opened, so a request outside the scope costs nothing and
    /// touches nothing. It is not the strong check -- see [`identities`](Self::identities).
    fn permits_request(&self, board: &str, probe: Option<&str>) -> bool {
        if !self.boards.is_empty() && !self.boards.iter().any(|b| b == board) {
            return false;
        }
        if !self.probes.is_empty() {
            let Some(probe) = probe else { return false };
            if !self.probes.iter().any(|p| p == probe) {
                return false;
            }
        }
        true
    }

    /// Whether a write over the Lamella Link may be asked for at all. Checked before anything is
    /// opened.
    ///
    /// **A LINK WRITE CAN SATISFY ONLY A `board:` TERM.** It reaches the board's own firmware over the
    /// board's serial or USB carrier, so it goes through no probe, and a `probe:` term cannot hold for
    /// it, just as it cannot for a flash that names no probe. And the chip id a board reports over
    /// the Link is not the reading a `chip:` term is checked against. That reading is the id the
    /// flashing contract takes from the part, a different register on each family: an RP2350's OTP
    /// chip id, an STM32's DEV_ID. A term that cannot be checked does not hold, so either kind refuses
    /// every Link write.
    fn permits_link_request(&self) -> Result<(), String> {
        let mut unchecked = Vec::new();
        if !self.probes.is_empty() {
            unchecked.push("probe:");
        }
        if !self.chips.is_empty() {
            unchecked.push("chip:");
        }
        if unchecked.is_empty() {
            return Ok(());
        }
        Err(format!(
            "This server may not write a board over the Lamella Link. It was started with: {}.\n\n\
A Lamella Link write goes through no probe, and the chip id a board reports over the Link is not \
the reading a chip: scope is checked against. So a {} scope cannot be checked for it, and nothing \
was opened. A board: scope is what narrows a Link write. That is set when the server starts.",
            self.describe(),
            unchecked.join(" or ")
        ))
    }

    /// Whether the board that answered a HELLO is one a `board:` term names.
    ///
    /// Checked after the HELLO and before anything is written, on the connection the write then
    /// crosses, so it is the board the write reaches. It compares the MODEL the board reports with
    /// the model each named board's own table declares. So a `board:` term names a model, and two
    /// boards of one model are both inside it. A board reporting model 0 is inside no `board:` term:
    /// 0 is what a firmware with no model to report sends.
    #[cfg_attr(not(feature = "bake"), allow(dead_code))]
    fn permits_link_board(&self, product_model: u16) -> Result<(), String> {
        if self.boards.is_empty() {
            return Ok(());
        }
        let named = product_model != lamella_wire::product_model::UNKNOWN
            && self.boards.iter().any(|id| {
                lamella_catalog::load_board(id)
                    .is_some_and(|board| board.product_model == i64::from(product_model))
            });
        if named {
            return Ok(());
        }
        let answered = if product_model == lamella_wire::product_model::UNKNOWN {
            "a board that reports no model".to_owned()
        } else {
            match product_model_name(product_model) {
                Some(name) => format!("{name} (product_model {product_model})"),
                None => format!("product_model {product_model}, which this build does not know"),
            }
        };
        Err(format!(
            "This server may not write this board: it answered as {answered}. It was started with: \
{}.\n\nNothing was written. That is set when the server starts.",
            self.describe()
        ))
    }

    /// The permission the flashing contract enforces against the part itself.
    fn identities(&self) -> lamella_flash_backend::Allow {
        if self.chips.is_empty() {
            lamella_flash_backend::Allow::Any
        } else {
            lamella_flash_backend::Allow::Identities(self.chips.clone())
        }
    }

    /// How to describe the scope in a refusal, so a reader can see what WOULD be allowed.
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.boards.is_empty() {
            parts.push(format!("boards {}", self.boards.join(", ")));
        }
        if !self.probes.is_empty() {
            parts.push(format!("probes {}", self.probes.join(", ")));
        }
        if !self.chips.is_empty() {
            let ids: Vec<String> = self.chips.iter().map(|c| format!("{c:#x}")).collect();
            parts.push(format!("chips {}", ids.join(", ")));
        }
        if parts.is_empty() {
            "any board this build can write".to_owned()
        } else {
            parts.join("; ")
        }
    }
}

/// The directory `--allow-write=<dir>` grants, or `None` when the server was started without it.
///
/// **A FILE A TOOL WRITES GOES ONLY HERE, AND ONLY AS A NEW FILE** ([`write_new_file`]). Without the
/// flag no tool writes a file at all, which is the default a server somebody found in a repository
/// should have. The directory must exist when the server starts, and is made absolute then, so a
/// later write cannot land somewhere the grant did not name.
fn parse_write_dir(args: impl Iterator<Item = String>) -> Result<Option<std::path::PathBuf>, String> {
    let mut granted = None;
    for arg in args {
        let Some(rest) = arg.strip_prefix("--allow-write") else { continue };
        let Some(dir) = rest.strip_prefix('=').filter(|dir| !dir.is_empty()) else {
            return Err(format!("{arg}: name the directory, as --allow-write=<dir>"));
        };
        if granted.is_some() {
            return Err("--allow-write names one directory, and it was given more than once".to_owned());
        }
        let path = std::path::Path::new(dir);
        if !path.is_dir() {
            return Err(format!("--allow-write={dir}: there is no directory there"));
        }
        let absolute = std::path::absolute(path).map_err(|error| format!("--allow-write={dir}: {error}"))?;
        granted = Some(absolute);
    }
    Ok(granted)
}

/// Why a file called `name` may not be created in `dir`, or the directory it may be created in.
///
/// `name` is a plain file name ending in `.lmli`: letters, digits, `-`, `_` and `.`, not starting
/// with a dot. That leaves out every directory part, `..`, a drive (`C:x`), an NTFS stream (`a:b`,
/// which writes beside a file already there), and control characters. A name Windows opens as a
/// device in every directory (`CON`, `NUL`, `COM1` and the rest, whatever follows the first dot) is
/// refused too.
#[cfg_attr(not(feature = "bake"), allow(dead_code))]
fn writable<'a>(dir: Option<&'a std::path::Path>, name: &str) -> Result<&'a std::path::Path, String> {
    let Some(dir) = dir else {
        return Err("this server writes no files: it was started without --allow-write=<dir>".to_owned());
    };
    let Some(stem) = name.strip_suffix(".lmli") else {
        return Err(format!("out_name {name:?} does not end in .lmli"));
    };
    let plain = !stem.is_empty()
        && name.len() <= 128
        && !stem.starts_with('.')
        && stem.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !plain {
        return Err(format!(
            "out_name {name:?} is not a plain file name: use letters, digits, '-', '_' and '.', with no \
             directory part"
        ));
    }
    let device = stem.split('.').next().unwrap_or_default().to_ascii_uppercase();
    let numbered = (device.starts_with("COM") || device.starts_with("LPT"))
        && device.len() == 4
        && device.as_bytes()[3].is_ascii_digit();
    if numbered || matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return Err(format!("out_name {name:?} is a name Windows reserves for a device"));
    }
    Ok(dir)
}

/// Write `bytes` as a NEW file called `name` in the `--allow-write` directory, and return its path.
///
/// **IT NEVER WRITES THROUGH ANYTHING ALREADY AT THAT NAME.** The file is created only if nothing is
/// there, so an existing file, and a link at the name, both refuse rather than being replaced or
/// followed. A write that fails part way removes the file it created.
#[cfg_attr(not(feature = "bake"), allow(dead_code))]
fn write_new_file(
    dir: Option<&std::path::Path>,
    name: &str,
    bytes: &[u8],
) -> Result<std::path::PathBuf, String> {
    let dir = writable(dir, name)?;
    let path = dir.join(name);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            format!("{name} is already in {}, and a new file never replaces one", dir.display())
        } else {
            format!("could not create {}: {error}", path.display())
        }
    })?;
    if let Err(error) = file.write_all(bytes) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(format!("could not write {}: {error}", path.display()));
    }
    Ok(path)
}

/// The protocol revisions this server speaks, newest first.
///
/// **`initialize` IS A NEGOTIATION AND WAS BEING ANSWERED AS AN ANNOUNCEMENT.** The client states
/// the revision it wants; a server that supports it answers with the SAME string, and one that does
/// not answers with a revision it does support so the client can decide whether to go on.
///
/// The wire surface here -- `tools/list`, `tools/call`, `resources/list`, `resources/read`, `ping`
/// -- is unchanged across these revisions, which is why one server can answer to all of them. A
/// revision that changed those shapes would need more than a string in this list.
const SUPPORTED_PROTOCOLS: [&str; 4] =
    [MODERN_PROTOCOL, "2025-06-18", "2025-03-26", "2024-11-05"];

/// The revision at which the protocol stopped having a handshake.
///
/// **THIS SERVER IS DUAL-ERA AND HAS TO BE.** A modern client puts the revision in every request's
/// `_meta` and never calls `initialize`; a legacy client calls `initialize` and expects a session.
/// Answering only one of them makes the other fail, and both are in the field.
const MODERN_PROTOCOL: &str = "2026-07-28";

/// The `_meta` key every modern request carries its revision in.
const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";

/// The `_meta` key a discovery result reports server identity under.
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// JSON-RPC error code for `UnsupportedProtocolVersionError`.
///
/// It is what tells a modern client it has reached a MODERN server that cannot speak its revision,
/// as opposed to a legacy one -- which is how a dual-era client decides whether to retry or fall
/// back. Returning a generic error here would make this server look legacy to everyone.
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// How long a client may treat a list or read result as fresh.
///
/// The tool contract and every resource are baked into this binary, so they cannot change while the
/// process runs. An hour is a freshness hint rather than a guarantee, and this one is honest.
const CACHE_TTL_MS: i64 = 3_600_000;

/// What `server/discover` tells a model this server is for.
///
/// **THE FIRST THING AN ASSISTANT READS ABOUT LAMELLA, and often the only thing before it starts
/// writing code.** It says what the tools do and names the two traps that cost a caller a wasted
/// turn: the language version everything compiles at, and the fact that a board is never inferred.
const DISCOVER_INSTRUCTIONS: &str = "Compile, run and deploy C# for small embedded targets. `lamella_check` and `lamella_run` need no hardware; `lamella_boards` lists what this build knows. Programs compile at C# 1 unless a verb says otherwise, so generics and `var` are language-version errors rather than missing types. `lamella_flash` ERASES a board and never infers which one -- an ambiguous bench is refused with its candidates listed, and you call again naming one.";

/// The revision to answer `initialize` with, given what the client asked for.
///
/// Echoes the client's request when it is one this server speaks; otherwise answers with the newest
/// one it does, which is what lets the client fail loudly rather than assume agreement.
///
fn negotiate_protocol(requested: Option<&str>) -> &'static str {
    match requested {
        Some(asked) => SUPPORTED_PROTOCOLS
            .into_iter()
            .find(|known| *known == asked)
            .unwrap_or(SUPPORTED_PROTOCOLS[0]),
        None => SUPPORTED_PROTOCOLS[0],
    }
}

const USAGE: &str = r#"
usage: lamella-mcp [--allow-device[=<scope>[,<scope>]]] [--allow-write=<dir>]
                   [--run-timeout=<seconds>]

An MCP server over stdio. It is started BY an MCP client, not by hand -- the client reads its
configuration and launches this process, so these arguments are written where that configuration
lives rather than typed. For a project-scoped client that is usually a file in the repository:

  {
    "mcpServers": {
      "lamella": {
        "command": "lamella-mcp",
        "args": ["--allow-device=chip:1234ABCD5678EF01"]
      }
    }
  }

WITHOUT --allow-device, every tool that touches a board is refused and the rest still work:
compiling, running on this machine, listing boards, and answering whether an image fits. That is
the useful default for a server somebody discovered in a repository and has not decided to trust.

WITH bare --allow-device, any board this build can write is writable.

The scopes narrow that. They bind every tool that writes a board: lamella_flash, which writes a
chip through a probe or a bootloader drive, and the three that write a board over the Lamella
Link, lamella_deploy, lamella_run_on_device and lamella_debug_launch. The tools that list and
identify boards need only --allow-device, because identifying a board is how a caller finds the
one in scope. The other debug tools act on a session lamella_debug_launch opened.

  board:<id>       a flash: narrows what may be ASKED FOR -- checked before any hardware is
                   opened. A Link write: the model the board reports when it answers, checked
                   before anything is written, so two boards of one model are both inside it
  probe:<serial>   a flash: the same, for the probe the write goes through. A Link write goes
                   through no probe, so a probe: scope refuses every one
  chip:<hex>       a flash: narrows what may be WRITTEN -- checked against the reading the PART
                   gave, after it identifies itself and before anything is erased. A board does
                   not report that reading over the Link, so a chip: scope refuses every Link write

chip: is only as narrow as the reading the part can give, and that varies by part. An RP2350
publishes a die-unique 64-bit OTP chip id, and there chip: is the only scope that survives somebody
moving a cable -- a probe serial names a cable and a board id names a family. An STM32L0 answers a
product CATEGORY every part in it shares, so one chip: scope covers every L0 of that category on
the bench: a third family check rather than a board one. The write reports which kind it got, in
those words. Worth setting when several boards are attached and only one is yours.

Terms of the same kind are alternatives; terms of different kinds must all hold. Repeating the
flag accumulates.

THIS IS A GUARD AGAINST MISTAKES AND NOT AN ACCESS CONTROL. Whoever can start this server can
start it with a wider scope, and can usually run the `lamella` CLI instead, which asks nobody.
It narrows what this surface will do; it does not narrow what the process can reach.

WITHOUT --allow-write, no tool writes a file. WITH --allow-write=<dir>, lamella_bake may keep the
image it builds as a NEW file in <dir>, named by its out_name: a plain name ending in .lmli. No
file already there is replaced, and a name with a directory part is refused. The directory must
exist when the server starts.

lamella_run runs each program in a process of its own and stops it after 20 seconds, or after
--run-timeout=<seconds> (1 to 3600). A program that never ends costs that process, not the
server. It may hold at most a million objects live at once; past that, an allocation raises
OutOfMemoryException.
"#;

fn main() {
    if std::env::args().nth(1).as_deref() == Some(RUN_CHILD) {
        run_child();
    }
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return;
    }
    let scope = DeviceScope::parse(std::env::args()).unwrap_or_else(|error| {
        eprintln!("lamella-mcp: {error}");
        std::process::exit(2)
    });
    let write_dir = parse_write_dir(std::env::args()).unwrap_or_else(|error| {
        eprintln!("lamella-mcp: {error}");
        std::process::exit(2)
    });
    let run_timeout = parse_run_timeout(std::env::args()).unwrap_or_else(|error| {
        eprintln!("lamella-mcp: {error}");
        std::process::exit(2)
    });
    let mut server = Server::new(scope, write_dir, run_timeout).unwrap_or_else(|error| {
        eprintln!("lamella-mcp: {error}");
        std::process::exit(1)
    });

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    while let Some(msg) = next_message(&mut input, &mut out) {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();

        if let Some(asked) = requested_protocol(&msg) {
            if !SUPPORTED_PROTOCOLS.contains(&asked) {
                if let Some(id) = &id {
                    respond_error_with_data(
                        &mut out,
                        id,
                        UNSUPPORTED_PROTOCOL_VERSION,
                        "Unsupported protocol version",
                        json!({ "supported": SUPPORTED_PROTOCOLS, "requested": asked }),
                    );
                }
                continue;
            }
        }

        match method {
            "server/discover" => {
                if let Some(id) = &id {
                    respond(
                        &mut out,
                        id,
                        with_cache_hints(json!({
                            "supportedVersions": SUPPORTED_PROTOCOLS,
                            "capabilities": { "tools": {}, "resources": {} },
                            "instructions": DISCOVER_INSTRUCTIONS,
                            "_meta": {
                                META_SERVER_INFO: {
                                    "name": "lamella-mcp",
                                    "version": "0.2.0"
                                }
                            }
                        })),
                    );
                }
            }
            "initialize" => {
                if let Some(id) = &id {
                    let asked = msg
                        .get("params")
                        .and_then(|p| p.get("protocolVersion"))
                        .and_then(Value::as_str);
                    respond(
                        &mut out,
                        id,
                        json!({
                            "protocolVersion": negotiate_protocol(asked),
                            "capabilities": { "tools": {}, "resources": {} },
                            "serverInfo": { "name": "lamella-mcp", "version": "0.2.0" }
                        }),
                    );
                }
            }
            "notifications/initialized" | "initialized" => {}
            "ping" => {
                if let Some(id) = &id {
                    respond(&mut out, id, json!({}));
                }
            }
            "tools/list" => {
                if let Some(id) = &id {
                    respond(&mut out, id, with_cache_hints(json!({ "tools": tools() })));
                }
            }
            "tools/call" => {
                if let Some(id) = &id {
                    match checked_call(msg.get("params")) {
                        Ok((name, arguments)) => respond(&mut out, id, server.call_tool(name, &arguments)),
                        Err(CallRefused::Tool(result)) => respond(&mut out, id, result),
                        Err(CallRefused::Protocol(code, message)) => respond_error(&mut out, id, code, &message),
                    }
                }
            }
            "resources/list" => {
                if let Some(id) = &id {
                    respond(&mut out, id, with_cache_hints(resources_list()));
                }
            }
            "resources/read" => {
                if let Some(id) = &id {
                    let uri = msg.get("params").and_then(|p| p.get("uri")).and_then(Value::as_str).unwrap_or_default();
                    match read_resource(uri) {
                        Some(r) => respond(&mut out, id, with_cache_hints(r)),
                        None => respond_error(&mut out, id, INVALID_PARAMS, &format!("no such resource: {uri}")),
                    }
                }
            }
            _ => {
                if let Some(id) = &id {
                    respond_error(&mut out, id, METHOD_NOT_FOUND, "method not found");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    mod a_deploy_start {
        use super::super::start_deployed;
        use std::time::Duration;

        /// A target that answers the first frame it is sent with the frames `answer` builds.
        struct Answering {
            wire: lamella_wire::MemTransport,
            answer: Option<Vec<u8>>,
        }

        impl Answering {
            fn with(answer: impl FnOnce(&mut lamella_wire::MemTransport)) -> Self {
                let mut peer = lamella_wire::MemTransport::new();
                answer(&mut peer);
                Answering { wire: lamella_wire::MemTransport::new(), answer: Some(peer.take_sent()) }
            }
        }

        impl lamella_wire::Transport for Answering {
            fn send(&mut self, _msg_type: u8, _seq: u16, _payload: &[u8]) -> Result<(), lamella_wire::TransportError> {
                if let Some(answer) = self.answer.take() {
                    self.wire.feed(&answer);
                }
                Ok(())
            }

            fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, lamella_wire::TransportError> {
                lamella_wire::Transport::poll(&mut self.wire)
            }
        }

        /// A board answering a start with the acknowledgement `code`.
        fn acknowledging(code: u8) -> Answering {
            Answering::with(|peer| {
                lamella_wire::Transport::send(peer, lamella_wire_host::exec::EXEC_ACK, 2, &[code]).unwrap();
            })
        }

        /// A deploy's answer calls its start made only when the board acknowledges it; a start the
        /// board refuses, or does not answer, makes the answer an error, with the board's reason.
        #[test]
        fn a_start_is_reported_as_made_only_when_the_board_acknowledges_it() {
            let quick = Duration::from_millis(50);
            let (said, is_error) = start_deployed(&mut acknowledging(lamella_wire_host::exec::ack::STARTED), quick);
            assert!(!is_error, "{said}");

            let (said, is_error) =
                start_deployed(&mut acknowledging(lamella_wire_host::exec::ack::NOTHING_TO_RUN), quick);
            assert!(is_error, "a refused start is an error: {said}");
            assert!(said.contains("NOTHING_TO_RUN"), "and says the board's reason: {said}");

            let (said, is_error) = start_deployed(&mut Answering::with(|_| {}), quick);
            assert!(is_error, "a start nobody acknowledged is not reported as made: {said}");
            assert!(!said.contains("Booted"), "{said}");
        }
    }

    #[cfg(feature = "bake")]
    mod a_deploy_outside_the_scope {
        use super::super::{DeviceScope, deploy_over};
        use lamella_wire::{MemTransport, Transport, TransportError, msg};
        use serde_json::{Value, json};
        use std::time::Duration;

        /// A board that answers its first frame with a HELLO_ACK naming `model`, and keeps the type
        /// of every frame it is sent.
        struct Board {
            inbox: MemTransport,
            hello_ack: Option<Vec<u8>>,
            sent: Vec<u8>,
        }

        impl Board {
            fn answering_as(model: u16) -> Self {
                let ack = lamella_wire::HelloAck {
                    chosen: lamella_wire::PROTOCOL_VERSION,
                    caps: lamella_wire::Capabilities(lamella_wire::Capabilities::BAKED_IMAGE),
                    identity: lamella_wire::TargetIdentity {
                        product_model: model,
                        ..lamella_wire::TargetIdentity::default()
                    },
                    max_inbound_payload: None,
                };
                let mut peer = MemTransport::new();
                peer.send(msg::HELLO_ACK, 0, &ack.encode()).expect("queue the HELLO_ACK");
                Board { inbox: MemTransport::new(), hello_ack: Some(peer.take_sent()), sent: Vec::new() }
            }
        }

        impl Transport for Board {
            fn send(&mut self, msg_type: u8, _seq: u16, _payload: &[u8]) -> Result<(), TransportError> {
                self.sent.push(msg_type);
                if let Some(answer) = self.hello_ack.take() {
                    self.inbox.feed(&answer);
                }
                Ok(())
            }

            fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, TransportError> {
                self.inbox.poll()
            }
        }

        fn text(answer: &Value) -> &str {
            answer["content"][0]["text"].as_str().expect("the answer is text")
        }

        fn scoped(term: &str) -> DeviceScope {
            DeviceScope::parse([format!("--allow-device={term}")].into_iter()).expect("the term parses")
        }

        /// **A DEPLOY TO A BOARD OUTSIDE THE SCOPE STOPS AT ITS HELLO**: nothing but the HELLO reaches
        /// the board, and the refusal names the board that answered.
        #[test]
        fn a_deploy_to_a_board_the_scope_does_not_name_stops_at_its_hello() {
            let v2 = lamella_wire::product_model::MICROBIT_V2;
            let quick = Duration::from_millis(50);

            let mut board = Board::answering_as(v2);
            let answer = deploy_over(&scoped("board:rpi-pico2"), &mut board, "COM0", &[0; 16], true, quick);
            assert_eq!(answer["isError"], json!(true));
            assert!(text(&answer).contains("may not write this board"), "{}", text(&answer));
            assert!(text(&answer).contains("BBC micro:bit v2"), "it names the board: {}", text(&answer));
            assert!(board.sent.iter().all(|sent| *sent == msg::HELLO), "only a HELLO was sent: {:?}", board.sent);

            let mut board = Board::answering_as(v2);
            let answer = deploy_over(&scoped("board:bbc-micro-bit-v2"), &mut board, "COM0", &[0; 16], true, quick);
            assert!(!text(&answer).contains("may not write"), "{}", text(&answer));
            assert!(board.sent.iter().any(|sent| *sent != msg::HELLO), "the deploy began: {:?}", board.sent);
        }

        /// A board that HELLOs with the prefix CRC, has room for any image, stores every chunk with
        /// its first byte wrong, and says so honestly: each acknowledgement carries the CRC of what it
        /// holds.
        #[derive(Default)]
        struct Miswriting {
            flash: Vec<u8>,
            replies: MemTransport,
        }

        impl Transport for Miswriting {
            fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
                use lamella_wire_host::deploy;
                let reply = match msg_type {
                    msg::HELLO => {
                        let ack = lamella_wire::HelloAck {
                            chosen: lamella_wire::PROTOCOL_VERSION,
                            caps: lamella_wire::Capabilities(
                                lamella_wire::Capabilities::BAKED_IMAGE | lamella_wire::Capabilities::DEPLOY_PREFIX_CRC,
                            ),
                            identity: lamella_wire::TargetIdentity::default(),
                            max_inbound_payload: None,
                        };
                        (msg::HELLO_ACK, ack.encode())
                    }
                    deploy::DEPLOY_STATUS => {
                        let mut status = vec![deploy::deploy_state::NONE, 0];
                        status.extend_from_slice(&0u64.to_le_bytes());
                        status.extend_from_slice(&65_536u32.to_le_bytes());
                        (deploy::DEPLOY_STATUS_RESULT, status)
                    }
                    _ => {
                        let offset = u32::from_le_bytes(payload[0..4].try_into().expect("an offset")) as usize;
                        let bytes = &payload[8..];
                        let end = offset + bytes.len();
                        self.flash.resize(self.flash.len().max(end), 0xFF);
                        self.flash[offset..end].copy_from_slice(bytes);
                        self.flash[offset] ^= 0x01;
                        let mut reply = vec![msg::xfer::MATCHED];
                        reply.extend_from_slice(&lamella_wire::crc32::of(&self.flash[..end]).to_le_bytes());
                        (deploy::XFER_RESULT, reply)
                    }
                };
                self.replies.feed(&lamella_wire::encode_frame(reply.0, seq, &reply.1).expect("a frame"));
                Ok(())
            }

            fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, TransportError> {
                self.replies.poll()
            }
        }

        /// **A DEPLOY COMPARES THE CRC EACH CHUNK'S ACKNOWLEDGEMENT CARRIES**, so a board that stored
        /// the image wrong is refused at that chunk, with both CRCs, rather than reported deployed.
        #[test]
        fn a_deploy_to_a_board_that_stored_it_wrong_is_refused_at_the_chunk() {
            let bare = DeviceScope::parse(["--allow-device".to_owned()].into_iter()).expect("parses");
            let answer = deploy_over(&bare, &mut Miswriting::default(), "COM0", &[0x5A; 1000], false, Duration::from_millis(200));
            assert_eq!(answer["isError"], json!(true), "{}", text(&answer));
            assert!(text(&answer).contains("does not hold the image that was sent"), "{}", text(&answer));
        }
    }

    use super::*;

    /// The boards resource must be COMPUTED from the wire's board table, not stored beside it.
    /// A stored copy is a second source of truth, and this one had gone five boards stale before
    /// anybody read it. Red-proof: serve `resources.json`'s stored `text` again and every board
    /// past the one it stopped at fails this assertion by name.
    #[test]
    fn the_boards_resource_names_every_board_the_wire_knows() {
        let body = read_resource("lamella://boards").expect("the boards resource exists");
        let text = body["contents"][0]["text"].as_str().expect("it has text").to_owned();

        let mut model: u16 = 0;
        while let Some(name) = lamella_wire::product_model::name(model) {
            assert!(
                text.contains(&format!("| {model} | {name} |")),
                "product_model {model} ({name}) is missing from the boards resource -- \
                 the resource is not deriving from lamella_wire::product_model"
            );
            model += 1;
        }
        assert!(model > 1, "the scan found no boards at all, so it proves nothing");
    }

    /// Every `lamella_*` name in this server's PROSE, checked against the contract.
    ///
    /// **A RESOURCE THAT NAMES A TOOL NOBODY ROUTES IS AN INSTRUCTION TO CALL SOMETHING THAT
    /// ANSWERS `unknown tool`.** The test below compares the contract against the dispatch -- two
    /// LISTS -- and is structurally blind to a sentence. `lamella://boards` spent its whole life
    /// telling every assistant to run a devices-enumeration verb under a name no contract has ever
    /// declared, and both directions of that cross-check passed throughout. The served text is
    /// GENERATED here rather than stored, so a check over `resources.json` could not have seen it
    /// either.
    ///
    #[test]
    fn every_tool_named_in_this_server_s_prose_is_one_the_contract_declares() {
        /// Each `lamella_*` token in `haystack`, and whether a `::` follows it.
        fn tokens(haystack: &str) -> Vec<(String, bool)> {
            let mut found = Vec::new();
            let mut rest = haystack;
            while let Some(at) = rest.find("lamella_") {
                let tail = &rest[at..];
                let end = tail
                    .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
                    .unwrap_or(tail.len());
                if end > "lamella_".len() {
                    found.push((tail[..end].to_owned(), tail[end..].starts_with("::")));
                }
                rest = &tail[end..];
            }
            found
        }

        let declared: Vec<String> = contract_tools()
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect();
        assert!(!declared.is_empty(), "the contract parsed to nothing, so this proves nothing");

        let haystacks = [include_str!("main.rs"), RESOURCES];
        let crates: Vec<String> = haystacks
            .iter()
            .flat_map(|haystack| tokens(haystack))
            .filter_map(|(name, is_path)| is_path.then_some(name))
            .collect();

        let mut scanned = 0;
        for (name, _) in haystacks.iter().flat_map(|haystack| tokens(haystack)) {
            if crates.contains(&name) {
                continue;
            }
            scanned += 1;
            assert!(
                declared.contains(&name),
                "`{name}` is named in this server's prose and the contract does not declare it, \
                 so an assistant told to call it gets `unknown tool`"
            );
        }
        assert!(scanned > 20, "the scan reached only {scanned} names, so it proves nothing");
    }

    /// **EVERY TOOL THE CONTRACT DECLARES MUST BE ONE THIS SERVER ROUTES, AND THE OTHER WAY ROUND.**
    /// The contract is a hand-authored JSON and the dispatch is a `match` in another file; a tool
    /// added to one and not the other is silent in both directions. A declared-but-unrouted tool
    /// tells an assistant a capability exists and then answers `unknown tool`, and a routed-but-
    /// undeclared one is invisible to every client, which for a protocol whose entire discovery
    /// mechanism is `tools/list` is the same as not having built it.
    ///
    #[test]
    fn the_contract_declares_exactly_the_tools_this_server_routes() {
        const ROUTED: [&str; 22] = [
            "lamella_version",
            "lamella_check",
            "lamella_run",
            "lamella_size",
            "lamella_boards",
            "lamella_fit",
            "lamella_reconcile",
            "lamella_bake",
            "lamella_list_devices",
            "lamella_identify_device",
            "lamella_run_on_device",
            "lamella_deploy",
            "lamella_deploy_status",
            "lamella_flash",
            "lamella_debug_launch",
            "lamella_debug_set_breakpoints",
            "lamella_debug_continue",
            "lamella_debug_step",
            "lamella_debug_stack",
            "lamella_debug_locals",
            "lamella_debug_eval",
            "lamella_debug_disconnect",
        ];
        let declared: Vec<String> = contract_tools()
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect();
        assert!(!declared.is_empty(), "the contract parsed to nothing, so this proves nothing");

        for name in ROUTED {
            assert!(
                declared.iter().any(|declared| declared == name),
                "`{name}` is routed and the contract does not declare it, so no client can call it"
            );
        }
        for name in &declared {
            assert!(
                ROUTED.contains(&name.as_str()),
                "the contract declares `{name}` and this server does not route it, so calling it \
                 answers `unknown tool`"
            );
        }
    }

    /// **THE C# SURFACE RESOURCE IS SERVED TO ASSISTANTS AS AUTHORITATIVE, SO ITS CLAIMS ARE
    /// CHECKABLE ONES.** `StringBuilder` compiles and runs, and `$"..."` interpolation is `CS8022`
    /// because this tool compiles at C# 1 -- each measured by compiling one program.
    ///
    /// A false NEGATIVE costs a reader code they could have written; a false POSITIVE hands them
    /// something to debug. This asserts by SECTION rather than by wording, so the prose can be
    /// rewritten freely and neither claim can quietly swap sides.
    #[test]
    fn the_surface_resource_does_not_repeat_the_claims_measurement_refuted() {
        let body = read_resource("lamella://bcl").expect("the surface resource exists");
        let text = body["contents"][0]["text"].as_str().expect("it has text");

        let supported = section(text, "## Supported today");
        let unavailable = section(text, "## NOT available yet");
        assert!(!supported.is_empty() && !unavailable.is_empty(), "the sections moved: {text}");

        assert!(
            supported.contains("StringBuilder"),
            "StringBuilder compiles and runs; it belongs in the supported list"
        );
        assert!(
            !unavailable.contains("StringBuilder"),
            "StringBuilder was listed as a compile error and it is not one"
        );
        assert!(
            !supported.contains("interpolation"),
            "$\"...\" is CS8022 at the version this tool compiles; claiming it hands a reader a bug"
        );
        assert!(
            !unavailable.contains("CS1056"),
            "interpolation is a language-version gate now, not an unexpected character"
        );
        assert!(
            text.contains("CS8022") && text.contains("langversion"),
            "the List<T> refusal must say it is a language-version gate, not a missing type"
        );

        let hardware = section(text, "## Hardware (GPIO)");
        assert!(!hardware.is_empty(), "the hardware section moved: {text}");
        assert!(
            !hardware.contains("CS0246"),
            "GPIO compiles through every verb now; naming that diagnostic sends a reader after a \
             problem that no longer exists"
        );
        assert!(
            hardware.contains("70") && hardware.contains("no pins"),
            "the page has to say WHY a compiling GPIO program produces nothing on the host"
        );
        let sample = hardware.split("```").nth(1).unwrap_or_default();
        assert!(
            !sample.contains("var "),
            "`var` is C# 3 and these verbs compile at C# 1, so a `var` sample is CS8022 first"
        );
        assert!(
            sample.contains("GpioController gpio"),
            "the sample has to declare the type explicitly to survive the C# 1 rung"
        );
    }

    /// **A SCOPE ON A REQUEST AND A SCOPE ON A DEVICE ARE DIFFERENT PROTECTIONS**, and the parser
    /// has to keep them apart, because only one of them survives somebody moving a cable.
    #[test]
    fn a_chip_scope_reaches_the_contract_and_a_board_scope_does_not() {
        let scope = DeviceScope::parse(
            ["--allow-device=chip:1234ABCD5678EF01".to_owned()].into_iter(),
        )
        .expect("a chip scope parses");
        assert!(scope.allowed);
        assert_eq!(
            scope.identities(),
            lamella_flash_backend::Allow::Identities(vec![0x1234_ABCD_5678_EF01]),
            "a chip scope must reach the contract, which is the only place it can be checked \
             against the PART"
        );

        let scope = DeviceScope::parse(["--allow-device=board:rpi-pico2".to_owned()].into_iter())
            .expect("a board scope parses");
        assert_eq!(
            scope.identities(),
            lamella_flash_backend::Allow::Any,
            "a board scope narrows the REQUEST and says nothing about which part may be erased"
        );
        assert!(scope.permits_request("rpi-pico2", None));
        assert!(!scope.permits_request("bbc-micro-bit-v2", None), "another board is outside it");
    }

    /// A probe scope requires a probe to be named -- otherwise "only through this cable" would be
    /// satisfied by a call that named no cable at all.
    #[test]
    fn a_probe_scope_is_not_satisfied_by_naming_no_probe() {
        let scope = DeviceScope::parse(["--allow-device=probe:SERIAL0000000005".to_owned()].into_iter())
            .expect("parses");
        assert!(scope.permits_request("rpi-pico2", Some("SERIAL0000000005")));
        assert!(!scope.permits_request("rpi-pico2", Some("SOMETHING-ELSE")));
        assert!(
            !scope.permits_request("rpi-pico2", None),
            "a call naming no probe cannot satisfy a scope that names one"
        );
    }

    /// Bare `--allow-device` still permits everything, because a single-board bench should not be
    /// made to write a scope it does not need.
    #[test]
    fn a_bare_flag_permits_any_board_and_no_flag_permits_none() {
        let bare = DeviceScope::parse(["--allow-device".to_owned()].into_iter()).expect("parses");
        assert!(bare.allowed);
        assert!(bare.permits_request("anything", None));
        assert_eq!(bare.identities(), lamella_flash_backend::Allow::Any);

        let none = DeviceScope::parse(["--something-else".to_owned()].into_iter()).expect("parses");
        assert!(!none.allowed, "hardware tools are refused when the flag is absent");
    }

    /// Repeating the flag ACCUMULATES, so a wrapper script appending one scope cannot silently
    /// widen another by replacing it.
    #[test]
    fn repeating_the_flag_accumulates_rather_than_replacing() {
        let scope = DeviceScope::parse(
            [
                "--allow-device=chip:AAAA".to_owned(),
                "--allow-device=chip:BBBB".to_owned(),
            ]
            .into_iter(),
        )
        .expect("parses");
        assert_eq!(
            scope.identities(),
            lamella_flash_backend::Allow::Identities(vec![0xAAAA, 0xBBBB])
        );
    }

    /// A malformed scope is a startup failure, not a silently-ignored argument. An unparsed scope
    /// that defaulted to "allow everything" would be the worst possible reading of a typo.
    #[test]
    fn a_malformed_scope_is_refused_rather_than_ignored() {
        assert!(DeviceScope::parse(["--allow-device=rpi-pico2".to_owned()].into_iter()).is_err());
        assert!(DeviceScope::parse(["--allow-device=chip:zzz".to_owned()].into_iter()).is_err());
        assert!(DeviceScope::parse(["--allow-device=colour:red".to_owned()].into_iter()).is_err());
        assert!(DeviceScope::parse(["--allow-device=board:rpi-pico-typo".to_owned()].into_iter()).is_err());
    }

    /// The scope a single `--allow-device=<term>` sets.
    fn scoped(term: &str) -> DeviceScope {
        DeviceScope::parse([format!("--allow-device={term}")].into_iter()).expect("the term parses")
    }

    /// **A LINK WRITE IS BOUND BY THE SCOPE, NOT ONLY A FLASH.** Under a `probe:` or a `chip:` term
    /// each of the three is refused before anything is opened, because neither term can be checked
    /// over the Link. Under a `board:` term, or bare `--allow-device`, each goes on to the board's
    /// HELLO, where the board term is checked.
    #[test]
    fn a_probe_or_chip_scope_refuses_every_link_write_before_anything_opens() {
        for tool in LINK_WRITES {
            for term in ["probe:SERIAL0000000005", "chip:1234ABCD5678EF01"] {
                let refusal = refusal_before_dispatch(&scoped(term), tool);
                if cfg!(feature = "bake") {
                    let refusal = refusal.unwrap_or_else(|| panic!("{tool} under {term} was not refused"));
                    assert_eq!(refusal["isError"], json!(true));
                    let text = refusal["content"][0]["text"].as_str().expect("the refusal is text");
                    assert!(text.contains("nothing was opened"), "{tool} under {term}: {text}");
                } else {
                    assert!(refusal.is_none(), "without bake {tool} is not built, and its dispatch says so");
                }
            }
            assert!(refusal_before_dispatch(&scoped("board:rpi-pico2"), tool).is_none(), "{tool}");
            let bare = DeviceScope::parse(["--allow-device".to_owned()].into_iter()).expect("parses");
            assert!(refusal_before_dispatch(&bare, tool).is_none(), "{tool}");
        }
        assert!(refusal_before_dispatch(&scoped("probe:SERIAL0000000005"), "lamella_flash").is_none());
    }

    /// A `board:` term binds a Link write to the MODEL the board reports in its HELLO, compared with
    /// the model the named board's own table declares.
    #[test]
    fn a_board_scope_binds_a_link_write_to_the_model_the_board_reports() {
        let model = |id: &str| {
            let declared = lamella_catalog::load_board(id).expect("a known board").product_model;
            u16::try_from(declared).expect("a wire model")
        };
        let (pico2, v2) = (model("rpi-pico2"), model("bbc-micro-bit-v2"));
        assert_ne!(pico2, v2, "the control needs two different models");

        let scope = scoped("board:rpi-pico2");
        assert!(scope.permits_link_board(pico2).is_ok());
        let refusal = scope.permits_link_board(v2).expect_err("another model is outside it");
        assert!(refusal.contains("BBC micro:bit v2"), "the refusal names the board that answered: {refusal}");
        assert!(
            scope.permits_link_board(lamella_wire::product_model::UNKNOWN).is_err(),
            "a board that reports no model is inside no board: term"
        );

        let bare = DeviceScope::parse(["--allow-device".to_owned()].into_iter()).expect("parses");
        assert!(bare.permits_link_board(v2).is_ok());
        assert!(bare.permits_link_board(lamella_wire::product_model::UNKNOWN).is_ok());
    }

    /// **EVERY DEVICE TOOL THE CONTRACT DOES NOT CALL READ-ONLY IS BOUND BY THE SCOPE.**
    ///
    /// Derived from the contract rather than from a list kept beside this one, so a tool that is
    /// added later is checked without anybody remembering this test. Bound means one of three: it is
    /// `lamella_flash`, with its own check; it is a [`LINK_WRITES`] tool; or it takes a `session`,
    /// which only `lamella_debug_launch` opens, inside the scope.
    #[test]
    fn every_device_tool_that_can_change_a_board_is_bound_by_the_scope() {
        let declared = contract_tools();
        for name in DEVICE_TOOLS {
            assert!(declared.iter().any(|tool| tool["name"] == name), "`{name}` is not in the contract");
        }
        let mut writable = 0;
        for tool in declared {
            let name = tool["name"].as_str().expect("a tool has a name");
            if !DEVICE_TOOLS.contains(&name) || tool["annotations"]["readOnlyHint"] == json!(true) {
                continue;
            }
            writable += 1;
            let takes_a_session = tool["inputSchema"]["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|field| field == "session"));
            assert!(
                name == "lamella_flash" || LINK_WRITES.contains(&name) || takes_a_session,
                "`{name}` can change a board and nothing binds it to --allow-device's scope"
            );
        }
        assert!(writable > LINK_WRITES.len(), "the contract marks only {writable} device tools writable");
    }

    /// A directory of its own for one test, under the system's temporary directory.
    fn scratch_dir(test: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let dir = std::env::temp_dir().join(format!("lamella-mcp-{test}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the test's directory");
        dir
    }

    /// `--run-timeout` is whole seconds from 1 to 3600, given once, and 20 without it.
    #[test]
    fn the_run_timeout_is_whole_seconds_given_once() {
        let args = |list: &[&str]| list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>().into_iter();
        assert_eq!(parse_run_timeout(args(&["--allow-device"])), Ok(DEFAULT_RUN_TIMEOUT));
        assert_eq!(parse_run_timeout(args(&["--run-timeout=5"])), Ok(Duration::from_secs(5)));
        for refused in ["--run-timeout", "--run-timeout=", "--run-timeout=0", "--run-timeout=3601", "--run-timeout=1.5"] {
            assert!(parse_run_timeout(args(&[refused])).is_err(), "{refused} must be refused");
        }
        assert!(parse_run_timeout(args(&["--run-timeout=5", "--run-timeout=9"])).is_err(), "given once");
    }

    /// `--allow-write` grants ONE directory, which must exist when the server starts.
    #[test]
    fn the_write_grant_is_one_directory_that_exists() {
        let args = |list: &[&str]| list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>().into_iter();
        assert_eq!(parse_write_dir(args(&["--allow-device"])).expect("no grant is fine"), None);

        let dir = scratch_dir("grant");
        let flag = format!("--allow-write={}", dir.display());
        let granted = parse_write_dir(args(&[flag.as_str()])).expect("an existing directory");
        assert!(granted.is_some_and(|granted| granted.is_absolute()), "the grant is held absolute");

        assert!(parse_write_dir(args(&["--allow-write"])).is_err(), "a grant names its directory");
        assert!(parse_write_dir(args(&["--allow-write="])).is_err());
        assert!(parse_write_dir(args(&[flag.as_str(), flag.as_str()])).is_err(), "one directory");
        let absent = format!("--allow-write={}", dir.join("absent").display());
        assert!(parse_write_dir(args(&[absent.as_str()])).is_err(), "a directory that is not there");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A FILE IS WRITTEN ONLY IN THE GRANTED DIRECTORY, ONLY AS A NEW FILE, AND ONLY UNDER A PLAIN
    /// `.lmli` NAME.** Every other name is refused before anything is opened.
    ///
    #[test]
    fn a_new_file_goes_only_in_the_granted_directory_and_never_over_one_already_there() {
        let dir = scratch_dir("write");
        assert!(write_new_file(None, "blink.lmli", b"x").is_err(), "no grant, no file");
        assert!(!dir.join("blink.lmli").exists());

        let written = write_new_file(Some(&dir), "blink.lmli", b"first").expect("a plain new name");
        assert_eq!(written, dir.join("blink.lmli"));
        assert_eq!(std::fs::read(&written).expect("it was written"), b"first");

        let again = write_new_file(Some(&dir), "blink.lmli", b"second").expect_err("the name is taken");
        assert!(again.contains("never replaces"), "{again}");
        assert_eq!(std::fs::read(&written).expect("it is still there"), b"first", "and unchanged");

        for name in [
            "../blink.lmli", "sub/blink.lmli", "sub\\blink.lmli", "C:blink.lmli", "C:\\blink.lmli",
            "/blink.lmli", "blink:stream.lmli", ".blink.lmli", ".lmli", "blink.txt", "blink", "",
            "bl ink.lmli", "CON.lmli", "nul.tar.lmli", "com1.lmli", "LPT9.lmli",
        ] {
            assert!(write_new_file(Some(&dir), name, b"x").is_err(), "{name:?} must be refused");
        }
        assert!(write_new_file(Some(&dir), "console.lmli", b"x").is_ok());
        assert!(write_new_file(Some(&dir), "com10.lmli", b"x").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `lamella_flash` refuses a `.lmli` and source by kind, naming what to use instead, and hands
    /// a chip image on to its route.
    #[test]
    fn the_flash_tool_refuses_a_link_payload_and_source_naming_what_to_use_instead() {
        let refusal = |image: &str| {
            refusal_unless_a_chip_image(image)
                .map(|refused| refused["content"][0]["text"].as_str().unwrap_or_default().to_owned())
        };
        let payload = refusal("blink.lmli").expect("a .lmli is not a chip image");
        assert!(payload.contains("lamella deploy") && payload.contains("lamella_deploy"), "{payload}");
        let source = refusal("Program.cs").expect("source is not a chip image");
        assert!(source.contains("lamella_deploy"), "{source}");
        assert!(refusal("serve.uf2").is_none() && refusal("image.bin").is_none(), "chip images pass");
    }

    /// **`tools/list` SERVES EXACTLY THE TOOLS THIS BUILD ROUTES**: the whole contract with `bake`,
    /// and the contract less the bake-only tools without it.
    #[test]
    fn the_tool_list_serves_exactly_the_tools_this_build_routes() {
        let names = |tools: &[Value]| -> Vec<String> {
            tools.iter().filter_map(|tool| tool["name"].as_str().map(str::to_owned)).collect()
        };
        let contract = names(contract_tools());
        for name in BAKE_ONLY {
            assert!(contract.iter().any(|declared| declared == name), "`{name}` is not in the contract");
        }
        let listed = tools();
        let listed = names(listed.as_array().expect("the list is an array"));
        let expected: Vec<String> = contract
            .iter()
            .filter(|name| cfg!(feature = "bake") || !BAKE_ONLY.contains(&name.as_str()))
            .cloned()
            .collect();
        assert_eq!(listed, expected);
        assert!(listed.len() < contract.len() || cfg!(feature = "bake"), "without bake, fewer are served");
    }

    /// **A CALL IS CHECKED AGAINST THE TOOL IT NAMES BEFORE IT RUNS.** A malformed request, or a tool
    /// this build does not serve, is a JSON-RPC error; arguments the tool's schema rules out are a
    /// tool result with `isError`, which the model can correct.
    #[test]
    fn a_call_is_checked_against_the_tool_it_names_before_it_runs() {
        let protocol = |params: Value| match checked_call(Some(&params)) {
            Err(CallRefused::Protocol(code, message)) => (code, message),
            Err(CallRefused::Tool(result)) => panic!("{params} was answered as a tool result: {result}"),
            Ok((name, _)) => panic!("{params} reached {name}"),
        };
        let refused = |params: Value| match checked_call(Some(&params)) {
            Err(CallRefused::Tool(result)) => {
                assert_eq!(result["isError"], json!(true));
                result["content"][0]["text"].as_str().unwrap_or_default().to_owned()
            }
            Err(CallRefused::Protocol(code, message)) => panic!("{params} was a protocol error {code}: {message}"),
            Ok((name, _)) => panic!("{params} reached {name}"),
        };

        assert_eq!(protocol(json!({})).0, INVALID_PARAMS, "a call names its tool");
        assert_eq!(protocol(json!({ "name": "no_such_tool" })), (INVALID_PARAMS, "Unknown tool: no_such_tool".to_owned()));
        let (_, why) = protocol(json!({ "name": "lamella_check", "arguments": [] }));
        assert!(why.contains("arguments must be an object"), "{why}");
        if !cfg!(feature = "bake") {
            let (_, why) = protocol(json!({ "name": "lamella_deploy", "arguments": {} }));
            assert!(why.starts_with("Unknown tool: lamella_deploy") && why.contains("--features bake"), "{why}");
        }

        let why = refused(json!({ "name": "lamella_fit", "arguments": { "board": "rpi-pico2", "image_bytes": -5 } }));
        assert!(why.contains("arguments.image_bytes must be 0 or more"), "{why}");
        let why = refused(json!({ "name": "lamella_check", "arguments": {} }));
        assert!(why.contains("arguments.code is required"), "{why}");

        let fit = json!({ "name": "lamella_fit", "arguments": { "board": "rpi-pico2", "image_bytes": 100 } });
        assert!(matches!(checked_call(Some(&fit)), Ok(("lamella_fit", _))));
        assert!(matches!(checked_call(Some(&json!({ "name": "lamella_version" }))), Ok(("lamella_version", _))));
    }

    /// **EVERY KEYWORD AND TYPE THE CONTRACT'S SCHEMAS USE IS ONE THE CHECKER ENFORCES OR IGNORES ON
    /// PURPOSE.** A keyword it did not know would be a constraint the contract promises and nothing
    /// checks.
    #[test]
    fn every_keyword_the_contract_uses_is_one_the_checker_knows() {
        fn walk(schema: &Value, keywords: &mut Vec<String>, types: &mut Vec<String>) {
            let Some(object) = schema.as_object() else { return };
            for (keyword, value) in object {
                keywords.push(keyword.clone());
                if keyword == "type" {
                    types.push(value.as_str().unwrap_or("(not a string)").to_owned());
                }
            }
            for property in object.get("properties").and_then(Value::as_object).into_iter().flat_map(|p| p.values()) {
                walk(property, keywords, types);
            }
            for nested in ["items", "additionalProperties"] {
                if let Some(inner) = object.get(nested) {
                    walk(inner, keywords, types);
                }
            }
        }
        let (mut keywords, mut types) = (Vec::new(), Vec::new());
        for tool in contract_tools() {
            walk(&tool["inputSchema"], &mut keywords, &mut types);
        }
        assert!(keywords.iter().any(|keyword| keyword == "minimum"), "the scan did not reach the properties");
        for keyword in &keywords {
            assert!(
                schema::CHECKED.contains(&keyword.as_str()) || schema::ANNOTATIONS.contains(&keyword.as_str()),
                "the contract uses `{keyword}`, which the argument checker does not enforce"
            );
        }
        for kind in &types {
            assert!(schema::TYPES.contains(&kind.as_str()), "the contract uses type `{kind}`, which the checker does not know");
        }
    }

    /// **A LINE THAT IS NOT A MESSAGE IS ANSWERED, AND THE SERVER GOES ON.** One that is not UTF-8
    /// ended the server, and one that was not JSON went unanswered.
    #[test]
    fn a_line_that_is_not_a_message_is_answered_and_the_server_goes_on() {
        let mut input = std::io::Cursor::new(
            b"\xff\xfe not text\n\
              not json\n\
              [1, 2]\n\
              {\"jsonrpc\":\"2.0\",\"id\":7}\n\
              {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}\n\
              \n\
              {\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n"
                .to_vec(),
        );
        let mut out = Vec::new();
        let message = next_message(&mut input, &mut out).expect("the ping after them is read");
        assert_eq!(message["method"], "ping");
        assert!(next_message(&mut input, &mut out).is_none(), "and then the input has ended");

        let answers: Vec<Value> = String::from_utf8(out)
            .expect("the answers are text")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each answer is JSON"))
            .collect();
        let codes: Vec<(Value, i64)> = answers
            .iter()
            .map(|answer| (answer["id"].clone(), answer["error"]["code"].as_i64().unwrap_or_default()))
            .collect();
        assert_eq!(
            codes,
            vec![
                (Value::Null, PARSE_ERROR),
                (Value::Null, PARSE_ERROR),
                (Value::Null, INVALID_REQUEST),
                (json!(7), INVALID_REQUEST),
            ],
            "a response needs no answer, and a blank line is not a message"
        );
    }

    /// **THE MODERN REVISION IS IN THE SUPPORTED LIST AND IS FIRST**, because a client with no
    /// preference is answered with the head of it.
    ///
    /// [`MODERN_PROTOCOL`] is the revision that removed the handshake: a client speaking it never
    /// calls `initialize` and puts its revision in every request's `_meta`. Claiming only legacy
    /// revisions would make this server unusable to one kind of client and invisible to the other.
    #[test]
    fn the_supported_list_leads_with_the_handshake_free_revision() {
        assert_eq!(
            SUPPORTED_PROTOCOLS[0], MODERN_PROTOCOL,
            "a client that states no preference gets the head of this list"
        );
        assert!(
            SUPPORTED_PROTOCOLS.contains(&MODERN_PROTOCOL),
            "the revision this server implements modern behavior for must be claimed"
        );
        assert!(SUPPORTED_PROTOCOLS.contains(&"2024-11-05"), "legacy clients are still served");
    }

    /// A request's revision comes from `_meta`, and its ABSENCE means legacy rather than invalid.
    ///
    /// Rejecting a request that carries no `_meta` would break every legacy client at once, which
    /// is the failure a dual-era server exists to avoid.
    #[test]
    fn a_request_without_meta_is_legacy_rather_than_malformed() {
        let modern = json!({
            "method": "tools/list",
            "params": { "_meta": { META_PROTOCOL_VERSION: "2026-07-28" } }
        });
        assert_eq!(requested_protocol(&modern), Some("2026-07-28"));

        let legacy = json!({ "method": "initialize", "params": { "protocolVersion": "2024-11-05" } });
        assert_eq!(
            requested_protocol(&legacy),
            None,
            "a legacy request declares no _meta revision and must not be read as one"
        );
        assert_eq!(requested_protocol(&json!({ "method": "ping" })), None);
    }

    /// **CACHING HINTS ARE REQUIRED ON A COMPLETE RESULT, NOT ADVISORY.** A client that receives
    /// none assumes the result is immediately stale, so omitting them does not fail loudly -- it
    /// makes every client re-fetch a list that cannot have changed.
    #[test]
    fn a_cacheable_result_carries_the_hints_the_specification_requires() {
        let hinted = with_cache_hints(json!({ "tools": [] }));
        assert_eq!(hinted["resultType"], "complete");
        assert_eq!(hinted["cacheScope"], "public", "nothing here varies per caller");
        let ttl = hinted["ttlMs"].as_i64().expect("ttlMs is an integer");
        assert!(ttl >= 0, "the specification requires ttlMs >= 0, got {ttl}");
        assert_eq!(hinted["tools"], json!([]), "and the payload survives");
    }

    /// **`initialize` IS A NEGOTIATION**, and answering it with one hard-coded string told every
    /// client its request had been honored whatever it asked for.
    #[test]
    fn initialize_answers_the_revision_the_client_asked_for_when_it_can() {
        for known in SUPPORTED_PROTOCOLS {
            assert_eq!(
                negotiate_protocol(Some(known)),
                known,
                "a revision this server speaks must be echoed, not overridden"
            );
        }
    }

    /// And when it cannot, it names one it does speak rather than agreeing silently -- which is
    /// what lets the client refuse instead of assuming a shape neither side has.
    #[test]
    fn an_unknown_revision_is_answered_with_one_this_server_actually_speaks() {
        let answer = negotiate_protocol(Some("1999-01-01"));
        assert!(
            SUPPORTED_PROTOCOLS.contains(&answer),
            "answered {answer}, which this server does not speak"
        );
        assert_ne!(answer, "1999-01-01", "it must not echo a revision it cannot honor");
        assert_eq!(negotiate_protocol(None), SUPPORTED_PROTOCOLS[0]);
    }

    /// **THE TOOL SURFACE IS STATELESS, AND THIS IS THE ASSERTION THAT KEEPS IT SO.**
    ///
    /// Each `lamella_run` program runs in a process of its own ([`run_in_child`]), so nothing one
    /// program declares or stores can reach the next call. What this guards is the reason: state
    /// that accumulated across tool calls would make a call's answer depend on which calls came
    /// before it, which no caller can see and no schema can describe.
    #[test]
    fn the_server_holds_no_state_a_caller_would_have_to_reason_about() {
        let source = include_str!("main.rs");
        let start = source.find("struct Server {").expect("the server struct exists");
        let end = source[start..].find("
}").expect("it closes") + start;
        let fields = &source[start..end];
        for accumulator in ["Vec<", "HashMap<", "VecDeque<", "BTreeMap<"] {
            assert!(
                !fields.contains(accumulator),
                "`Server` gained a {accumulator}...> field. If it accumulates across tool calls, a \
                 caller's answer now depends on calls it cannot see: {fields}"
            );
        }
    }

    /// The lines under a `##` heading, up to the next one.
    fn section<'a>(text: &'a str, heading: &str) -> &'a str {
        let Some(start) = text.find(heading) else { return "" };
        let rest = &text[start + heading.len()..];
        match rest.find("\n## ") {
            Some(end) => &rest[..end],
            None => rest,
        }
    }

    /// The three verdict tools answer from the SHARED catalog and the SHARED rule, which is the
    /// only reason a board cannot mean one thing to `lamella fit` and another to `lamella_fit`.
    /// Asserting a real board resolves here is what proves the crate is actually wired in -- an
    /// empty catalog would answer "no board" to everything, which reads exactly like a typo.
    #[test]
    fn the_verdict_tools_read_the_same_catalog_the_cli_does() {
        assert!(
            lamella_catalog::BOARDS.iter().any(|(id, _)| *id == "rpi-pico2"),
            "the shared catalog is not reaching this crate"
        );
        let (board, part) = lamella_catalog::resolve("rpi-pico2").expect("a known board");
        let verdict = lamella_bsp_gen::fit::fit(&board, &part, 100_000);
        assert!(
            matches!(verdict.flash_fit, lamella_bsp_gen::fit::Fit::Fits { .. }),
            "100 KB must fit an RP2350's flash: {:?}",
            verdict.flash_fit
        );
        assert!(!verdict.assumptions.is_empty(), "a verdict must say what it took as given");
        assert!(!verdict.not_answered.is_empty(), "and what it cannot answer");
    }

    /// `lamella_version` answers with the same numbers the CLI prints, from the same statement.
    ///
    /// **TWO FRONT ENDS THAT DISAGREE ABOUT WHAT A BUILD SUPPORTS IS WORSE THAN NEITHER ANSWERING**,
    /// which is the failure `lamella-flash-routes` was lifted out to prevent for board routing and
    /// this extends to the contracts. They share one `Contracts`, so the only way they could
    /// diverge is a hand-typed number here -- which is exactly what this refuses.
    #[test]
    fn the_version_tool_reports_the_contracts_and_not_a_copy_of_them() {
        let it = lamella_flash_routes::contracts::Contracts::of(env!("CARGO_PKG_VERSION"));
        let rendered = tool_version();
        let text = rendered
            .get("content")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .expect("the tool returns text");
        let body: Value = serde_json::from_str(text).expect("and the text is JSON");

        assert_eq!(body["link_protocol"], it.wire_protocol);
        assert_eq!(body["sidecar_schema"], it.sidecar_schema);
        assert_eq!(body["boards"], it.boards);
        assert_eq!(body["flashable"], it.flashable);
        assert_eq!(body["reads_as"], it.describe());
        assert_ne!(it.boards, it.flashable);
    }

    /// `lamella_boards` answers the flash question for EVERY row, from the routing table itself.
    ///
    /// **TWO LISTINGS CAN AGREE ON EVERY ROW AND STILL ANSWER DIFFERENT QUESTIONS**, which is the
    /// drift this checks and a shared catalog does not prevent by itself: membership came from
    /// `lamella_catalog` all along, and the flash column is what a caller asked for.
    ///
    /// The population assert is the load-bearing one. A `can_flash` that answered `false` for
    /// everything would satisfy a per-row check and be useless, and it reads exactly like a crate
    /// that is not wired in.
    #[test]
    fn the_board_listing_answers_the_flash_question_the_cli_column_answers() {
        let listing = tool_boards();
        let rendered = serde_json::to_string(&listing).expect("the listing serializes");
        assert!(rendered.contains("can_flash"), "the flash column is missing from the listing");

        let routable =
            lamella_catalog::BOARDS.iter().filter(|(id, _)| lamella_flash_routes::can_flash(id));
        assert!(routable.count() > 0, "no board is routable, so this column proves nothing");

        let unrouted = lamella_catalog::BOARDS
            .iter()
            .filter(|(id, _)| !lamella_flash_routes::can_flash(id))
            .count();
        assert!(unrouted > 0, "every board is routable, so the false side proves nothing");

        assert!(lamella_flash_routes::can_flash("rpi-pico2"), "a Pico 2 has a route");
    }
}
