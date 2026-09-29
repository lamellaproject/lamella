//! Which corlib a firmware embeds as its resident corlib when the build names none: the one
//! whose surface matches the runtime tier the firmware is built with, so a full runtime embeds the
//! every-feature corlib and the floor embeds the kernel-tier corlib.
//!
//! A resident corlib is the class library a deployed program resolves against on the board, and the
//! two can disagree safely in one direction only. A corlib narrower than its runtime refuses what it
//! lacks, by name. A corlib wider than its runtime is wrong without a word: a `[RuntimeProvided]`
//! method whose intrinsic the runtime was built without keeps its placeholder body and returns 0.
//! That is why the default follows the tier instead of being one file for every build.
//!
//! Set `LAMELLA_CORLIB_IMAGE` to embed a different corlib; the default applies only when it is unset.

// Each build script uses only the tiers its own features can select, so an unused tier is expected.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// The runtime surface a firmware is built with, which decides the corlib it pairs with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeTier {
    /// Every surface the runtime implements (the runner's `profile-full`), paired with the
    /// every-feature corlib that `build-managed.ps1` builds by default.
    Full,
    /// The floor (the runner's `profile-minimal`: the intrinsic BCL, exceptions and the collector),
    /// paired with the kernel-tier corlib, which is built with no surface symbols at all.
    Kernel,
}

/// Where `tier`'s default corlib may be, relative to a firmware crate's manifest directory, in the order
/// they are tried.
pub fn candidates(tier: RuntimeTier) -> &'static [&'static str] {
    match tier {
        // In a checkout that carries this repository's tests, the committed corlib they load.
        // Otherwise, what `build-managed.ps1` writes by default: the first step of a build from
        // these sources, and the corlib a program for the board is compiled against.
        RuntimeTier::Full => &[
            "../lamella-load/tests/fixtures/corlib.dll",
            "../../managed/corlib.dll",
        ],
        RuntimeTier::Kernel => &["../../tools/device-poc/corlib-kernel.dll"],
    }
}

/// The first of `tier`'s candidates that `exists` accepts, joined onto `manifest_dir`.
pub fn default_corlib(
    manifest_dir: &Path,
    tier: RuntimeTier,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidates(tier)
        .iter()
        .map(|candidate| manifest_dir.join(candidate))
        .find(|path| exists(path))
}

/// Points `LAMELLA_CORLIB_IMAGE` at `tier`'s default corlib when the cargo feature whose build-script
/// variable is `feature_variable` is on and the build has not named a corlib itself.
///
/// With no candidate present the variable stays unset, so the firmware's `env!` names the variable a
/// builder has to set rather than a path that does not exist. A missing default and a missing file
/// are different problems, and only one of them is the builder's to fix.
pub fn default_resident_corlib(feature_variable: &str, tier: RuntimeTier) {
    if std::env::var(feature_variable).is_ok() && std::env::var("LAMELLA_CORLIB_IMAGE").is_err() {
        let manifest_dir =
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
        if let Some(corlib) = default_corlib(&manifest_dir, tier, |path| path.exists()) {
            println!("cargo:rustc-env=LAMELLA_CORLIB_IMAGE={}", corlib.display());
        }
    }
    println!("cargo:rerun-if-env-changed=LAMELLA_CORLIB_IMAGE");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holds(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn a_full_runtime_takes_the_committed_corlib_first_then_the_built_one_then_nothing() {
        let manifest_dir = Path::new("crates/lamella-serve-example");
        assert_eq!(
            default_corlib(manifest_dir, RuntimeTier::Full, |_| true),
            Some(manifest_dir.join("../lamella-load/tests/fixtures/corlib.dll"))
        );
        assert_eq!(
            default_corlib(manifest_dir, RuntimeTier::Full, |path| path
                .ends_with("managed/corlib.dll")),
            Some(manifest_dir.join("../../managed/corlib.dll"))
        );
        assert_eq!(
            default_corlib(manifest_dir, RuntimeTier::Full, |_| false),
            None
        );
    }

    /// A full corlib under the floor's runtime is the silent direction, so no candidate may serve both.
    #[test]
    fn no_file_is_a_candidate_for_both_tiers() {
        for kernel in candidates(RuntimeTier::Kernel) {
            assert!(
                !candidates(RuntimeTier::Full).contains(kernel),
                "{kernel} serves both tiers"
            );
        }
    }

    /// What this tree's defaults embed, read from the files: the full tier's carries `Span<T>`, which
    /// the I2C and SPI facades of `System.Device.Gpio` take, and the kernel tier's does not. A tree
    /// without a candidate file has nothing to check here and passes.
    #[test]
    fn the_defaults_in_this_tree_carry_the_surface_of_their_tier() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        if let Some(full) = default_corlib(manifest_dir, RuntimeTier::Full, |path| path.exists()) {
            let bytes = std::fs::read(&full).expect("the full default reads");
            assert!(
                holds(&bytes, b"Span`1"),
                "{} has no Span<T>",
                full.display()
            );
        }
        if let Some(kernel) =
            default_corlib(manifest_dir, RuntimeTier::Kernel, |path| path.exists())
        {
            let bytes = std::fs::read(&kernel).expect("the kernel default reads");
            assert!(
                !holds(&bytes, b"Span`1"),
                "{} carries Span<T>",
                kernel.display()
            );
        }
    }
}
