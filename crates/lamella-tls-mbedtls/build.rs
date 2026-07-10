//! Compiles the vendored mbedTLS (vendor/mbedtls, pinned -- see README.md) plus the C shim
//! against csrc/lamella_mbedtls_config.h. On a bare-metal target the compiler is an ARM
//! cross GCC: `LAMELLA_ARM_GCC` if set, else `arm-none-eabi-gcc` on PATH, else the MSYS2
//! default install location. Host builds use the platform C compiler (the same one the
//! workspace's other native deps already require).

use std::path::{Path, PathBuf};
use std::process::Command;

/// The MSYS2 package location this machine class installs the ARM toolchain to; a plain
/// fallback after `LAMELLA_ARM_GCC` and PATH.
const MSYS2_ARM_GCC: &str = r"C:\msys64\ucrt64\bin\arm-none-eabi-gcc.exe";

fn arm_gcc() -> PathBuf {
    if let Ok(explicit) = std::env::var("LAMELLA_ARM_GCC") {
        return PathBuf::from(explicit);
    }
    let on_path = Command::new("arm-none-eabi-gcc").arg("--version").output();
    if on_path.map(|out| out.status.success()).unwrap_or(false) {
        return PathBuf::from("arm-none-eabi-gcc");
    }
    if Path::new(MSYS2_ARM_GCC).exists() {
        return PathBuf::from(MSYS2_ARM_GCC);
    }
    panic!(
        "no ARM cross C compiler found for a bare-metal target: set LAMELLA_ARM_GCC, or put \
         arm-none-eabi-gcc on PATH (msys2: pacman -S mingw-w64-ucrt-x86_64-arm-none-eabi-gcc)"
    );
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
            unsafe { std::env::set_var("PATH", joined) };
        }
        let ar = gcc.with_file_name(
            gcc.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.replace("gcc", "ar"))
                .unwrap_or_else(|| "arm-none-eabi-ar".into()),
        );
        build
            .compiler(&gcc)
            .archiver(&ar)
            .define("LAMELLA_FREESTANDING_LIBC", None)
            .flag("-fno-builtin")
            .flag(format!("-mcpu={}", arm_cpu(&target)))
            .flag("-mthumb")
            .flag("-mfloat-abi=soft")
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
