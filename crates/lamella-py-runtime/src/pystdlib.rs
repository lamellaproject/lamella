//! The bundled managed stdlib -- the pure-Python modules that ship WITH the runtime.

use alloc::vec::Vec;
use lamella_py_bytecode::{Capability, Profile};

/// Every bundled module's name, sorted. The set a host can offer without a filesystem.
pub const BUNDLED_MODULES: &[&str] = &[
    "array",
    "asyncio",
    "bisect",
    "copy",
    "functools",
    "heapq",
    "itertools",
    "json",
    "lamella",
    "lamella.clock",
    "operator",
    "os",
    "random",
    "re",
    "socket",
    "string",
    "struct",
    "sys",
    "threading",
    "time",
];

/// `asyncio`, with its coordination layer appended when the tier carries one.
///
/// **One MODULE assembled from two files, rather than two modules.** CPython puts `Lock`, `Queue`,
/// `wait_for` and `timeout` in `asyncio` itself, so a program reaching them anywhere else stops
/// being a program CPython can run -- and the subset promise is that ours run there unchanged. The
/// split is therefore about what a BOARD carries and never about what a program writes.
///
/// The concatenation is compile-time, so the tier that turns the knob off does not carry the
/// source at all: the bytes are not in the image and the names are not in the namespace.
#[cfg(all(feature = "bundled-stdlib", feature = "asyncio-sync", feature = "asyncio-streams"))]
const ASYNCIO: &str = concat!(
    include_str!("../pystdlib/asyncio.py"),
    include_str!("../pystdlib/asyncio_sync.py"),
    include_str!("../pystdlib/asyncio_streams.py"),
);

/// `asyncio` with the coordination layer and no streams.
#[cfg(all(feature = "bundled-stdlib", feature = "asyncio-sync", not(feature = "asyncio-streams")))]
const ASYNCIO: &str = concat!(
    include_str!("../pystdlib/asyncio.py"),
    include_str!("../pystdlib/asyncio_sync.py"),
);

/// `asyncio` with streams and no coordination layer -- the two knobs are independent, so all four
/// combinations exist and each is spelled out rather than assembled, because `concat!` takes
/// literals and a build that names an arm nobody wrote fails at the `ASYNCIO` reference with no
/// hint of which pair it was.
#[cfg(all(feature = "bundled-stdlib", not(feature = "asyncio-sync"), feature = "asyncio-streams"))]
const ASYNCIO: &str = concat!(
    include_str!("../pystdlib/asyncio.py"),
    include_str!("../pystdlib/asyncio_streams.py"),
);

/// `asyncio` without its coordination layer -- the loop, futures, tasks, `sleep`, `gather`, `run`.
///
/// What a program loses is stated rather than discovered: `Lock`, `Event`, `Semaphore`,
/// `BoundedSemaphore`, `Queue`, `wait_for` and `timeout` are absent, so a program using one gets an
/// `AttributeError` naming it instead of a board that imports and then runs out of memory.
#[cfg(all(feature = "bundled-stdlib", not(feature = "asyncio-sync"), not(feature = "asyncio-streams")))]
const ASYNCIO: &str = include_str!("../pystdlib/asyncio.py");

/// The PUBLIC native modules -- built by the interpreter itself rather than carried as source.
///
/// The private seams (`_thread`, `_reactor`, `_socket`, `_sys`, `_time`, `_fs`, `_struct`,
/// `_platform`) are deliberately absent: they are the underscore-prefixed layer the bundled modules
/// are written OVER, and CPython does not offer those to a reader either. What a program imports is
/// `threading`, not `_thread`.
const PUBLIC_NATIVE_MODULES: &[&str] = &["collections", "math", "weakref"];

/// Which capability a module needs beyond being present -- the modules whose SOURCE ships but whose
/// first line reaches a native seam a knob can remove.
///
/// >>> ONE ENTRY TODAY, AND THE SHAPE IS THE POINT. `threading.py` is ordinary Python a bundle
/// carries like any other module, so nothing about the artifact says it is unavailable -- but its
/// first line is `import _thread`, and that seam is native code the `threading` feature compiles
/// out. No artifact can supply it. So this is the one place where "the module resolves" and "the
/// program can run" come apart, and a caller that only asked the resolver would get it wrong. <<<
const MODULE_CAPABILITIES: &[(&str, Capability)] = &[("threading", Capability::Threading)];

/// Every module a program can import against `profile`, sorted -- the bundled sources plus the
/// public native modules, minus what the profile cannot provide.
///
/// # Why this lives here and takes a profile
///
/// It is the ONE function that can answer it. The bundled list and the native registry are both in
/// this crate; a front end cannot see either (it depends on this crate's siblings, not on it) and a
/// tool that assembled its own list would be maintaining a copy of two lists it does not own. Same
/// argument as [`crate::profile_of_this_build`]: one function, in the crate that owns the facts,
/// cannot disagree with itself.
///
/// **The profile is an ARGUMENT and never this build's own.** A host tool asking what a DEVICE can
/// import must pass the device's profile; answering from `cfg!` here would describe the machine the
/// tool was compiled on, which is the failure that makes a feature-gated change look free when it is
/// measured in a build that does not enable the feature.
///
/// An editor offering a module this refuses is an editor that suggests a program the target cannot
/// run -- the same rule the completion engine states for names, one level up at the import.
#[must_use]
pub fn importable_modules(profile: Profile) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = BUNDLED_MODULES
        .iter()
        .chain(PUBLIC_NATIVE_MODULES)
        .copied()
        .filter(|name| {
            MODULE_CAPABILITIES
                .iter()
                .find(|(module, _)| module == name)
                .is_none_or(|(_, capability)| profile.supports(*capability))
        })
        .collect();
    names.sort_unstable();
    names
}

/// One bundled module's Python source, or `None` when this runtime carries no module of that name.
///
/// The shape a bundling front end wants: pass it (wrapped to own its result) as the import resolver.
#[must_use]
#[cfg(feature = "bundled-stdlib")]
pub fn bundled_module(name: &str) -> Option<&'static str> {
    Some(match name {
        "array" => include_str!("../pystdlib/array.py"),
        "asyncio" => ASYNCIO,
        "bisect" => include_str!("../pystdlib/bisect.py"),
        "copy" => include_str!("../pystdlib/copy.py"),
        "functools" => include_str!("../pystdlib/functools.py"),
        "heapq" => include_str!("../pystdlib/heapq.py"),
        "itertools" => include_str!("../pystdlib/itertools.py"),
        "json" => include_str!("../pystdlib/json.py"),
        "lamella" => include_str!("../pystdlib/lamella_package.py"),
        "lamella.clock" => include_str!("../pystdlib/lamella_clock.py"),
        "operator" => include_str!("../pystdlib/operator.py"),
        "os" => include_str!("../pystdlib/os.py"),
        "random" => include_str!("../pystdlib/random.py"),
        "re" => include_str!("../pystdlib/re.py"),
        "socket" => include_str!("../pystdlib/socket.py"),
        "string" => include_str!("../pystdlib/string.py"),
        "struct" => include_str!("../pystdlib/struct.py"),
        "sys" => include_str!("../pystdlib/sys.py"),
        "threading" => include_str!("../pystdlib/threading.py"),
        "time" => include_str!("../pystdlib/time.py"),
        _ => return None,
    })
}

/// Without the `bundled-stdlib` feature the sources are not compiled in, so every name misses and a
/// program importing one gets the same `ModuleNotFoundError` it would on a host that offers none.
#[must_use]
#[cfg(not(feature = "bundled-stdlib"))]
pub fn bundled_module(_name: &str) -> Option<&'static str> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "bundled-stdlib")]
    #[test]
    fn every_listed_module_resolves_and_parses() {
        for name in BUNDLED_MODULES {
            let source = bundled_module(name).expect("a listed module resolves");
            assert!(!source.is_empty(), "{name} is empty");
            lamella_py_frontend::compile_str(name, source)
                .unwrap_or_else(|e| panic!("bundled {name} does not compile: {e}"));
        }
    }

    /// Every name [`importable_modules`] offers must actually import, and the two registries it
    /// unions are the ones that drift.
    ///
    /// A catalogue is a THIRD list beside the bundled sources and the native registry, so the
    /// failure it invites is offering a module that no longer resolves -- which reaches a developer
    /// as an editor suggesting an import that then fails. Walked rather than spot-checked.
    #[test]
    fn every_module_the_catalogue_offers_actually_resolves() {
        let profile = crate::profile_of_this_build();
        let offered = importable_modules(profile);
        assert!(offered.len() >= 10, "the catalogue reader is broken, not the registries");
        let mut model = crate::ObjectModel::new(alloc::vec::Vec::new(), 1024 * 1024);
        for name in &offered {
            let native = crate::stdlib::build_module(name, &mut model).is_some();
            let bundled = bundled_module(name).is_some();
            assert!(
                native || bundled,
                "`import {name}` is offered by the catalogue and resolves to nothing"
            );
        }
        for name in PUBLIC_NATIVE_MODULES {
            assert!(offered.contains(name), "{name} is a native module the catalogue does not offer");
        }
    }

    /// The capability filter, from BOTH sides.
    ///
    /// `threading` is the entry that matters: its source ships either way, so a catalogue that asked
    /// only the resolver would offer it to a target whose `_thread` seam was compiled out -- an
    /// editor suggesting a program that cannot run. A one-sided assertion would pass with the filter
    /// deleted.
    #[test]
    fn a_module_whose_native_seam_is_absent_is_not_offered() {
        let with = Profile::FULL;
        let without = Profile::FULL.without(Capability::Threading);
        assert!(importable_modules(with).contains(&"threading"));
        assert!(!importable_modules(without).contains(&"threading"));
        let dropped: alloc::vec::Vec<_> = importable_modules(with)
            .into_iter()
            .filter(|name| !importable_modules(without).contains(name))
            .collect();
        assert_eq!(dropped, alloc::vec!["threading"]);
    }

    #[test]
    fn an_unknown_name_misses_rather_than_failing() {
        assert!(bundled_module("board").is_none());
        assert!(bundled_module("no-such-module").is_none());
    }
}
