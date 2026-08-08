//! The bundled managed stdlib -- the pure-Python modules that ship WITH the runtime.

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
    "operator",
    "os",
    "random",
    "re",
    "string",
    "struct",
    "sys",
    "time",
];

/// One bundled module's Python source, or `None` when this runtime carries no module of that name.
///
/// The shape a bundling front end wants: pass it (wrapped to own its result) as the import resolver.
#[must_use]
#[cfg(feature = "bundled-stdlib")]
pub fn bundled_module(name: &str) -> Option<&'static str> {
    Some(match name {
        "array" => include_str!("../pystdlib/array.py"),
        "asyncio" => include_str!("../pystdlib/asyncio.py"),
        "bisect" => include_str!("../pystdlib/bisect.py"),
        "copy" => include_str!("../pystdlib/copy.py"),
        "functools" => include_str!("../pystdlib/functools.py"),
        "heapq" => include_str!("../pystdlib/heapq.py"),
        "itertools" => include_str!("../pystdlib/itertools.py"),
        "json" => include_str!("../pystdlib/json.py"),
        "operator" => include_str!("../pystdlib/operator.py"),
        "os" => include_str!("../pystdlib/os.py"),
        "random" => include_str!("../pystdlib/random.py"),
        "re" => include_str!("../pystdlib/re.py"),
        "string" => include_str!("../pystdlib/string.py"),
        "struct" => include_str!("../pystdlib/struct.py"),
        "sys" => include_str!("../pystdlib/sys.py"),
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

    #[test]
    fn an_unknown_name_misses_rather_than_failing() {
        assert!(bundled_module("socket").is_none());
        assert!(bundled_module("board").is_none());
    }
}
