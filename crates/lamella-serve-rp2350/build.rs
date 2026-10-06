//! Hands the bare-metal link its memory layout. Host builds (the stub main) link normally.

#[path = "../lamella-serve-core/src/resident_corlib.rs"]
mod resident_corlib;

/// The XIP window the part boots from.
const FLASH_ORIGIN: usize = 0x1000_0000;

fn main() {
    println!("cargo:rerun-if-changed=memory-rp2350.x");
    println!("cargo:rerun-if-changed=src/rp2350_flash.rs");
    let whole_flash_bytes = board_flash_bytes(board());
    // `rp2350_flash.rs` includes this for `FLASH_END`, so the deploy window runs to the end of
    // the flash this board solders rather than to one board's figure written down for all four.
    let generated =
        std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("board_flash.rs");
    std::fs::write(
        &generated,
        format!(
            "/// The board's flash in bytes, from `[memory] flash` in its `board.toml`.\n\
             const BOARD_FLASH_BYTES: usize = {whole_flash_bytes:#x};\n"
        ),
    )
    .unwrap_or_else(|e| panic!("cannot write {}: {e}", generated.display()));
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        // One family map: XIP flash at 0x10000000 with the PICOBIN IMAGE_DEF in the first
        // 4 KB (the bootrom scans for it; no stage-2 boot2 on this part, unlike the RP2040).
        //
        // The length is not the part's flash: it is how much of it this firmware may occupy.
        // A firmware built with `serve` shares the part with the deployed-image region and is
        // bounded by that region's base, so an oversized one fails at link time instead of
        // linking cleanly and being silently overwritten by the first deploy. A firmware that
        // takes no deploys (the wasm bring-up image) never writes that region and owns the
        // board's whole flash.
        // Keyed on `serve` rather than on the other bins' features so that a build carrying
        // both takes the safe bound -- the deploy region is live whenever `serve` is.
        let length = if std::env::var("CARGO_FEATURE_SERVE").is_ok() {
            deploy_region_base(whole_flash_bytes) - FLASH_ORIGIN
        } else {
            whole_flash_bytes
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
    std::fs::write(&out, format!("const PY_HEAP_BYTES: usize = {bytes};\n"))
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

/// The board this build is for, named by its `bsp/` folder. The features pick it, exactly as they
/// pick the firmware's board bindings and the identity it answers a host with.
fn board() -> &'static str {
    let on = |feature: &str| std::env::var_os(format!("CARGO_FEATURE_{feature}")).is_some();
    assert!(
        !(on("PICO_PLUS_2") && on("CYW43")),
        "the Pimoroni Pico Plus 2 has no radio, so `pico-plus-2` cannot be built with `cyw43`; \
         the Pico Plus 2 W's feature is `pico-plus-2-w`"
    );
    if on("PICO_PLUS_2_W") {
        "pimoroni-pico-plus-2-w"
    } else if on("PICO_PLUS_2") {
        "pimoroni-pico-plus-2"
    } else if on("CYW43") {
        "rpi-pico2-w"
    } else {
        "rpi-pico2"
    }
}

/// The flash `board` solders, in bytes: `[memory] flash` in its `board.toml`. The RP2350 has no
/// flash inside it -- it executes in place from a QSPI part on the board -- so the figure is the
/// board's: 4 MB on the Pico 2 and Pico 2 W, 16 MB on the Pimoroni Pico Plus 2 and Plus 2 W.
///
/// A board file this cannot read, or one that states no flash, fails the build: a default here
/// would be one board's figure applied to another, which is the fault this reads the facts to avoid.
fn board_flash_bytes(board: &str) -> usize {
    let facts = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../bsp")
        .join(board)
        .join("board.toml");
    println!("cargo:rerun-if-changed={}", facts.display());
    let text = std::fs::read_to_string(&facts)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", facts.display()));
    let mut in_memory = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') {
            in_memory = line == "[memory]";
            continue;
        }
        let Some(value) = in_memory
            .then(|| line.strip_prefix("flash")?.trim_start().strip_prefix('='))
            .flatten()
        else {
            continue;
        };
        let value = value.trim().replace('_', "");
        let bytes = value
            .strip_prefix("0x")
            .map_or_else(|| value.parse().ok(), |hex| usize::from_str_radix(hex, 16).ok())
            .unwrap_or_else(|| panic!("{}: `flash = {value}` is not a byte count", facts.display()));
        assert!(bytes > 0, "{} states no flash, and this board executes from it", facts.display());
        return bytes;
    }
    panic!("{} states no `flash` in its [memory] table", facts.display());
}
