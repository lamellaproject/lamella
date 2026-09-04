//! Hands the crate the one fact about its own build that no `cfg` in its source can state: the
//! rustc TARGET TRIPLE it is being compiled for.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo::rustc-env=LAMELLA_TARGET_TRIPLE={target}");
    println!("cargo::rerun-if-changed=build.rs");
}
