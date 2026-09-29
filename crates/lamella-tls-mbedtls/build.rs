//! Compiles the vendored mbedTLS (vendor/mbedtls, pinned -- see README.md) plus the C shim
//! against csrc/lamella_mbedtls_config.h. On a bare-metal target the compiler is an ARM
//! cross GCC: `LAMELLA_ARM_GCC` if set, else `arm-none-eabi-gcc` on PATH, and the archiver its
//! `-ar` sibling unless `LAMELLA_ARM_AR` names one. Host builds use the
//! platform C compiler (the same one the
//! workspace's other native deps already require).

use std::path::{Path, PathBuf};
use std::process::Command;

fn arm_gcc() -> PathBuf {
    if let Ok(explicit) = std::env::var("LAMELLA_ARM_GCC") {
        return PathBuf::from(explicit);
    }
    let on_path = Command::new("arm-none-eabi-gcc").arg("--version").output();
    if on_path.map(|out| out.status.success()).unwrap_or(false) {
        return PathBuf::from("arm-none-eabi-gcc");
    }
    panic!(
        "no ARM cross C compiler found for a bare-metal target: set LAMELLA_ARM_GCC, or put \
         arm-none-eabi-gcc on PATH (msys2: pacman -S mingw-w64-ucrt-x86_64-arm-none-eabi-gcc)"
    );
}

/// The float ABI the Rust target uses, which the C objects must share: hard on an `eabihf` target,
/// whose FPU the `cc` crate already names with `-mfpu`, and soft on every other.
fn arm_float_abi(target: &str) -> &'static str {
    if target.ends_with("eabihf") { "-mfloat-abi=hard" } else { "-mfloat-abi=soft" }
}

/// The -mcpu matching the Rust target's architecture floor.
fn arm_cpu(target: &str) -> &'static str {
    if target.starts_with("thumbv7em") {
        "cortex-m4"
    } else if target.starts_with("thumbv6m") {
        "cortex-m0plus"
    } else if target.starts_with("thumbv8m.main") {
        "cortex-m33"
    } else {
        "cortex-m3"
    }
}

fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    let bare_metal = target.contains("-none-");

    let mut build = cc::Build::new();
    build
        .include("vendor/mbedtls/include")
        .include("vendor/mbedtls/library")
        .include("csrc")
        .define("MBEDTLS_CONFIG_FILE", "\"lamella_mbedtls_config.h\"")
        .file("csrc/lamella_tls_shim.c");

    let library = Path::new("vendor/mbedtls/library");
    let mut sources: Vec<PathBuf> = std::fs::read_dir(library)
        .expect("vendor/mbedtls/library exists")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "c"))
        .collect();
    sources.sort();
    for source in sources {
        build.file(source);
    }

    if bare_metal {
        let gcc = arm_gcc();
        if let Some(bin_dir) = gcc.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            let existing = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![bin_dir.to_path_buf()];
            paths.extend(std::env::split_paths(&existing));
            let joined = std::env::join_paths(paths).expect("PATH entries join");
            #[allow(unsafe_code)]
            unsafe {
                std::env::set_var("PATH", joined)
            };
        }
        println!("cargo:rerun-if-env-changed=LAMELLA_ARM_AR");
        let ar = std::env::var_os("LAMELLA_ARM_AR").map(PathBuf::from).unwrap_or_else(|| {
            gcc.with_file_name(
                gcc.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.replace("gcc", "ar"))
                    .unwrap_or_else(|| "arm-none-eabi-ar".into()),
            )
        });
        build
            .compiler(&gcc)
            .archiver(&ar)
            .define("LAMELLA_FREESTANDING_LIBC", None)
            .flag("-fno-builtin")
            .flag(format!("-mcpu={}", arm_cpu(&target)))
            .flag("-mthumb")
            .flag(arm_float_abi(&target))
            .flag("-Os")
            .flag("-ffunction-sections")
            .flag("-fdata-sections")
            .flag("-fno-common");
    }

    build.compile("lamella_mbedtls");
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-changed=vendor/mbedtls/library");
    println!("cargo:rerun-if-changed=vendor/mbedtls/include");
    println!("cargo:rerun-if-env-changed=LAMELLA_ARM_GCC");
}
