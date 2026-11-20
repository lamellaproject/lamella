//! Hands the bare-metal link its memory layout. Host builds (the stub main) link normally.

#[path = "../lamella-serve-core/src/resident_corlib.rs"]
mod resident_corlib;

/// The XIP window the part boots from.
const FLASH_ORIGIN: usize = 0x1000_0000;

fn main() {
    println!("cargo:rerun-if-changed=memory-rp2350.x");
    println!("cargo:rerun-if-changed=src/rp2350_flash.rs");
    println!("cargo:rerun-if-changed=src/rp2350_settings.rs");
    let serves_csharp = std::env::var_os("CARGO_FEATURE_SERVE").is_some();
    let serves_python = std::env::var_os("CARGO_FEATURE_PYTHON_SERVE").is_some();
    assert!(
        !(serves_csharp && serves_python),
        "`serve` builds the C# firmware and `python-serve` the Python one, so give one of them"
    );
    // The target this firmware is built for, which a Python serve's HELLO names.
    println!("cargo:rustc-env=LAMELLA_TARGET_TRIPLE={}", std::env::var("TARGET").expect("TARGET"));
    let board = board();
    let facts = board_facts(board);
    check_psram(board, &facts);
    let whole_flash_bytes = board_flash_bytes(board, &facts);
    // `rp2350_settings.rs` includes this for `FLASH_END`, so the board's settings and the deploy
    // window below them end where the flash this board solders ends, rather than at one board's
    // figure written down for all of them.
    let generated =
        std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("board_flash.rs");
    std::fs::write(
        &generated,
        format!(
            "/// The board's flash in bytes, from the flash region its `board.toml` states.\n\
             const BOARD_FLASH_BYTES: usize = {whole_flash_bytes:#x};\n"
        ),
    )
    .unwrap_or_else(|e| panic!("cannot write {}: {e}", generated.display()));
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        // One family map: XIP flash at 0x10000000 with the PICOBIN IMAGE_DEF in the first
        // 4 KB (the bootrom scans for it; no stage-2 boot2 on this part, unlike the RP2040).
        //
        // The length is not the part's flash: it is how much of it this firmware may occupy.
        // A firmware built with `serve` or `python-serve` (hygiene-allow: a cargo feature, not a crate)
        // shares the part with the deployed-program region and is bounded by that region's base, so
        // an oversized one fails at link time instead of linking cleanly and being silently
        // overwritten by the first deploy. A firmware
        // that takes no deploys never writes that region, and owns the board's flash up to its
        // settings: those belong to the board, which keeps its stored Wi-Fi network there across
        // firmware updates, so an image written over them would erase it.
        // Keyed on the features that take deploys rather than on the bins', so that a build
        // carrying another bin too takes the safe bound -- the region is live whenever one is.
        let length = if serves_csharp || serves_python {
            deploy_region_base(whole_flash_bytes) - FLASH_ORIGIN
        } else {
            whole_flash_bytes - settings_bytes()
        };

        let template = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("memory-rp2350.x");
        let text = std::fs::read_to_string(&template)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", template.display()));
        // Refuse rather than emit an unbounded script: a silently unsubstituted template would
        // hand the linker the literal token and fail with a message about neither cause.
        assert!(
            text.contains("@LAMELLA_FLASH_LENGTH@"),
            "{} no longer carries the @LAMELLA_FLASH_LENGTH@ token, so the flash bound would \
             not be applied; restore it rather than hard-coding a length",
            template.display()
        );
        assert!(
            text.contains("@LAMELLA_STACK_BYTES@"),
            "{} no longer carries the @LAMELLA_STACK_BYTES@ token, so the stack's share of RAM \
             would not be set; restore it rather than hard-coding one",
            template.display()
        );
        let script = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR"))
            .join("memory-rp2350.x");
        std::fs::write(
            &script,
            text.replace("@LAMELLA_FLASH_LENGTH@", &format!("{length:#x}"))
                .replace("@LAMELLA_STACK_BYTES@", &format!("{:#x}", stack_bytes())),
        )
        .unwrap_or_else(|e| panic!("cannot write {}: {e}", script.display()));
        // Every bare-metal image this package links takes this memory map: the firmware binaries,
        // and the examples when the package has an `examples/` directory. Cargo refuses an examples
        // instruction from a package with no example target, so it is emitted only then.
        println!("cargo:rustc-link-arg-bins=-T{}", script.display());
        if std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").is_dir() {
            println!("cargo:rustc-link-arg-examples=-T{}", script.display());
        }
    }
    // A `resident-corlib` build embeds a corlib through include_bytes!(env!("LAMELLA_CORLIB_IMAGE")).
    // The `rp2350` part feature builds the full runtime, so the default is the every-feature corlib.
    resident_corlib::default_resident_corlib(
        "CARGO_FEATURE_RESIDENT_CORLIB",
        resident_corlib::RuntimeTier::Full,
    );
    if std::env::var("CARGO_FEATURE_PYTHON").is_ok() {
        embed_python_bundle();
    }
    #[cfg(feature = "cyw43")]
    provide_wifi_images();
}

/// The Wi-Fi radio's images for a `cyw43` build, in `OUT_DIR/wifi-images`: downloaded from the
/// project that publishes them at a pinned commit and checked against pinned hashes, or taken from
/// the folder `LAMELLA_WIFI_IMAGES_DIR` names for a build with no network, checked the same way. The
/// firmware carries them with `include_bytes!`, and the version the firmware must answer rides in
/// `LAMELLA_WIFI_VERSION`. A build that cannot provide them stops here and says why.
#[cfg(feature = "cyw43")]
fn provide_wifi_images() {
    println!("cargo:rerun-if-env-changed={}", lamella_wifi_images::IMAGES_DIR_ENV);
    let dest = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("wifi-images");
    let prefetched = std::env::var_os(lamella_wifi_images::IMAGES_DIR_ENV).map(std::path::PathBuf::from);
    if let Err(error) = lamella_wifi_images::provide(&dest, prefetched.as_deref()) {
        panic!("the Wi-Fi radio's images could not be provided: {error}");
    }
    println!("cargo:rustc-env=LAMELLA_WIFI_VERSION={}", lamella_wifi_images::VERSION);
}

/// Places the Python bundle the `rp2350-python-interpreter` firmware runs where that firmware can
/// include it.
///
/// `LAMELLA_PY_BUNDLE` names a bundle file (what the `py-bundle` tool writes); with the variable
/// unset the firmware gets an empty placeholder instead, which is the arm that measures the
/// interpreter's flash cost alone -- a bundle costs its own byte length, accounted separately from
/// the interpreter that runs it.
fn embed_python_bundle() {
    println!("cargo:rerun-if-env-changed=LAMELLA_PY_BUNDLE");
    let out = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("bundle.lpyc");
    match std::env::var("LAMELLA_PY_BUNDLE") {
        Ok(source) if !source.is_empty() => {
            println!("cargo:rerun-if-changed={source}");
            let bytes = std::fs::read(&source).unwrap_or_else(|e| {
                panic!("LAMELLA_PY_BUNDLE names {source}, which did not read: {e}")
            });
            std::fs::write(&out, bytes).expect("write the embedded bundle");
        }
        _ => std::fs::write(&out, [0u8; 64]).expect("write the placeholder bundle"),
    }
    embed_python_heap_size();
}

/// Fixes the object heap the Python firmware reserves out of its arena.
///
/// `LAMELLA_PY_HEAP_BYTES` overrides the default. It is a build-time constant rather than a run-time
/// one because the reservation happens before the program runs, and because a size measurement that
/// could be changed after linking would not be measuring the binary it flashed.
fn embed_python_heap_size() {
    println!("cargo:rerun-if-env-changed=LAMELLA_PY_HEAP_BYTES");
    const DEFAULT_HEAP_BYTES: usize = 32 * 1024;
    let bytes = std::env::var("LAMELLA_PY_HEAP_BYTES")
        .ok()
        .filter(|value| !value.is_empty())
        .map_or(DEFAULT_HEAP_BYTES, |value| {
            value.parse().unwrap_or_else(|e| {
                panic!("LAMELLA_PY_HEAP_BYTES is {value}, which is not a byte count: {e}")
            })
        });
    let out = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("py_heap.rs");
    std::fs::write(&out, format!("pub(crate) const PY_HEAP_BYTES: usize = {bytes};\n"))
        .expect("write the heap-size constant");
}

/// The stack's share of RAM, at its top. The C# firmware's heap takes what the statics leave below
/// it, and the core's stack-limit register guards the line between the two.
///
/// A `resident-corlib` build, which loads a deployed program's PE on the board, keeps 96 KiB. Every
/// other build runs baked images and keeps 64 KiB.
fn stack_bytes() -> usize {
    if std::env::var_os("CARGO_FEATURE_RESIDENT_CORLIB").is_some() {
        96 * 1024
    } else {
        64 * 1024
    }
}

/// Reads `IMAGE_BASE` out of the flash-region module, so the firmware's link ceiling and the
/// region it must stay clear of are one fact rather than two numbers that happen to agree.
///
/// Moving the region without moving the ceiling would allow exactly the failure the ceiling
/// exists to refuse, and nothing would say so -- so a base this cannot find or parse fails the
/// build instead of falling back to a permissive default.
fn deploy_region_base(whole_flash_bytes: usize) -> usize {
    let module = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/rp2350_flash.rs");
    let text = std::fs::read_to_string(&module)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", module.display()));
    let literal = text
        .lines()
        .find_map(|line| line.split_once("pub const IMAGE_BASE: usize =")?.1.split(';').next())
        .unwrap_or_else(|| {
            panic!(
                "{} no longer declares `pub const IMAGE_BASE: usize = ...;`, so the serve's \
                 flash ceiling cannot be derived from the region it must stay clear of",
                module.display()
            )
        })
        .trim()
        .replace('_', "");
    let base = literal
        .strip_prefix("0x")
        .and_then(|hex| usize::from_str_radix(hex, 16).ok())
        .unwrap_or_else(|| panic!("IMAGE_BASE is not a hex literal this can read: {literal:?}"));
    assert!(
        base > FLASH_ORIGIN && base <= FLASH_ORIGIN + whole_flash_bytes,
        "IMAGE_BASE {base:#x} is not inside this part's XIP flash window"
    );
    base
}

/// The board's settings in bytes -- `SETTINGS_SECTORS` sectors of `SECTOR_BYTES` -- read from
/// `src/rp2350_settings.rs`, where the firmware states their place, so the bound of a firmware that
/// takes no deploys moves with them. A value this cannot find or parse fails the build, as for
/// [`deploy_region_base`].
fn settings_bytes() -> usize {
    let module = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/rp2350_settings.rs");
    let text = std::fs::read_to_string(&module)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", module.display()));
    let constant = |name: &str| -> usize {
        let declaration = format!("pub const {name}: usize =");
        let literal = text
            .lines()
            .find_map(|line| line.split_once(declaration.as_str())?.1.split(';').next())
            .unwrap_or_else(|| {
                panic!(
                    "{} no longer declares `{declaration} ...;`, so the flash bound of a firmware \
                     that takes no deploys cannot be kept below the board's settings",
                    module.display()
                )
            })
            .trim()
            .replace('_', "");
        literal
            .parse()
            .unwrap_or_else(|_| panic!("{name} is not a decimal literal this can read: {literal:?}"))
    };
    constant("SETTINGS_SECTORS") * constant("SECTOR_BYTES")
}

/// A board's radio, as the firmware's `cyw43` feature sees it.
#[derive(PartialEq)]
enum Radio {
    /// The board has none.
    None,
    /// The CYW43439 on the Pico 2 W's four lines, GP23, GP24, GP25 and GP29, the lines the Wi-Fi
    /// driver runs.
    OnTheDriversLines,
    /// A radio on other lines, which the driver does not run: they are named here.
    Elsewhere(&'static str),
}

/// Each board feature, by the name a build script sees it under, with its board's folder in
/// `bsp/boards/` and its radio. The firmware's own board blocks pick the same board from the same
/// features, and it checks at compile time that the flash this sized it by is its board's.
const NAMED_BOARDS: &[(&str, &str, Radio)] = &[
    ("PICO_PLUS_2", "pimoroni-pico-plus-2", Radio::None),
    ("PICO_PLUS_2_W", "pimoroni-pico-plus-2-w", Radio::OnTheDriversLines),
    ("PICO_LIPO_2", "pimoroni-pico-lipo-2", Radio::None),
    ("PICO_LIPO_2_XL_W", "pimoroni-pico-lipo-2-xl-w", Radio::OnTheDriversLines),
    ("FEATHER_RP2350_HSTX", "adafruit-feather-rp2350-hstx", Radio::None),
    ("FEATHER_RP2350_HSTX_PSRAM", "adafruit-feather-rp2350-hstx-psram", Radio::None),
    ("METRO_RP2350", "adafruit-metro-rp2350", Radio::None),
    ("METRO_RP2350_PSRAM", "adafruit-metro-rp2350-psram", Radio::None),
    ("IOT_REDBOARD_RP2350", "sparkfun-iot-redboard-rp2350", Radio::Elsewhere("GP24, GP36, GP37 and GP38")),
];

/// The board this build is for, named by its folder in `bsp/boards/`. The features pick it, exactly
/// as they pick the firmware's board bindings and the identity it answers a host with: a board
/// feature names its board, and with none the board is the Pico 2, or with `cyw43` the Pico 2 W.
fn board() -> &'static str {
    let on = |feature: &str| std::env::var_os(format!("CARGO_FEATURE_{feature}")).is_some();
    let feature_name = |feature: &str| feature.to_ascii_lowercase().replace('_', "-");
    let named: Vec<_> = NAMED_BOARDS.iter().filter(|(feature, ..)| on(feature)).collect();
    if let [_, _, ..] = named[..] {
        let features: Vec<String> = named.iter().map(|(feature, ..)| feature_name(feature)).collect();
        panic!("a firmware is built for one board, and this build names {}", features.join(", "));
    }
    let Some(&&(feature, folder, ref radio)) = named.first() else {
        return if on("CYW43") { "raspberrypi-pico-2-w" } else { "raspberrypi-pico-2" };
    };
    if on("CYW43") {
        let with_radio: Vec<String> = NAMED_BOARDS
            .iter()
            .filter(|(.., radio)| *radio == Radio::OnTheDriversLines)
            .map(|(feature, ..)| format!("`{}`", feature_name(feature)))
            .collect();
        match radio {
            Radio::OnTheDriversLines => {}
            Radio::None => panic!(
                "{folder} has no radio, so `{}` cannot be built with `cyw43`; the board features \
                 with the radio are {}, and with no board feature `cyw43` builds the Pico 2 W",
                feature_name(feature),
                with_radio.join(" and ")
            ),
            Radio::Elsewhere(lines) => panic!(
                "{folder}'s radio is on {lines}, and the Wi-Fi driver runs the radio on the Pico 2 \
                 W's four lines only, GP23, GP24, GP25 and GP29, so `{}` cannot be built with \
                 `cyw43`: this board's firmware has no Wi-Fi",
                feature_name(feature)
            ),
        }
    }
    if on("PYTHON") {
        panic!(
            "the Python firmware is built for the Pico 2 and, with `python-net`, the Pico 2 W, so it \
             takes no board feature, and this build names `{}`",
            feature_name(feature)
        );
    }
    folder
}

/// `board`'s facts, read by bsp-gen's own parser from its `board.toml`. A file this cannot read
/// fails the build.
fn board_facts(board: &str) -> lamella_bsp_gen::strata::BoardTable {
    use lamella_bsp_gen::strata::{self, Strata};

    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let facts = lamella_bsp_gen::boards::board_dir(&repo, board).join("board.toml");
    println!("cargo:rerun-if-changed={}", facts.display());
    let text = std::fs::read_to_string(&facts)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", facts.display()));
    let Strata::Board(table) =
        strata::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", facts.display()))
    else {
        panic!("{} is not a board's facts", facts.display());
    };
    table
}

/// The flash `board` solders, in bytes: the flash region its facts state, which bsp-gen's parser
/// takes in either shape a board file writes it. The RP2350 has no flash inside it -- it executes
/// in place from a QSPI part on the board -- so the figure is the board's: 4 MB on the Pico 2 and
/// Pico 2 W, 8 MB on the two Feathers and 16 MB on the other boards.
///
/// A board whose facts state no flash fails the build: a default here would be one board's figure
/// applied to another, which is the fault this reads the facts to avoid.
fn board_flash_bytes(board: &str, facts: &lamella_bsp_gen::strata::BoardTable) -> usize {
    let bytes = facts.xip_flash();
    assert!(bytes > 0, "{board} states no flash, and this board executes from it");
    usize::try_from(bytes).unwrap_or_else(|_| panic!("{board}: {bytes} bytes of flash"))
}

/// Holds `psram-heap` to the board's facts, the Pico 2's and Pico 2 W's among them: the PSRAM joins
/// the heap exactly when they name the part the board fits, since the bring-up is told that part's
/// commands and timing. A board feature that left `psram-heap` out would leave a named PSRAM unused,
/// and one that added it would bring up a part nothing names.
fn check_psram(board: &str, facts: &lamella_bsp_gen::strata::BoardTable) {
    let named = facts
        .memory
        .iter()
        .find(|region| region.kind == "ram" && !region.controller.is_empty() && !region.part.is_empty());
    let on = std::env::var_os("CARGO_FEATURE_PSRAM_HEAP").is_some();
    match named {
        Some(region) => assert!(
            on,
            "{board}'s facts name its PSRAM, the {}, and this build has no `psram-heap`, so the PSRAM \
             would sit unused; its board feature pulls `psram-heap`",
            region.part
        ),
        None => assert!(
            !on,
            "{board}'s facts name no PSRAM part, so `psram-heap` has no part to bring up"
        ),
    }
}
