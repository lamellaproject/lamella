//! Compile-time exception-type tags for the typed lowering.

use alloc::vec;
use alloc::vec::Vec;

use lamella_py_bytecode::{EXCEPTION_HIERARCHY, exception_tag};

/// Whether `name` is a built-in Python exception type (one the shared hierarchy defines).
pub fn is_builtin_exception(name: &str) -> bool {
    EXCEPTION_HIERARCHY.iter().any(|(n, _)| *n == name)
}

/// The tags a `catch`/`except` of `name` matches: `name`'s own tag plus the tag of every type that
/// derives from it -- a thrown subtype the handler must take. Empty when `name` is not a built-in
/// exception (so the caller can fall the whole function to the dynamic path). The hierarchy lists
/// each base BEFORE its children, so one forward pass accumulates the entire descendant set.
pub fn subtype_tags(name: &str) -> Vec<u32> {
    if !is_builtin_exception(name) {
        return Vec::new();
    }
    let mut names: Vec<&str> = vec![name];
    for &(ty, base) in EXCEPTION_HIERARCHY {
        if names.contains(&base) && !names.contains(&ty) {
            names.push(ty);
        }
    }
    names.iter().map(|n| exception_tag(n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_names_are_builtin_exceptions() {
        assert!(is_builtin_exception("IndexError"));
        assert!(is_builtin_exception("ZeroDivisionError"));
        assert!(!is_builtin_exception("NotAnException"));
        assert!(!is_builtin_exception("print"));
    }

    #[test]
    fn a_leaf_type_matches_only_itself() {
        assert_eq!(subtype_tags("IndexError"), vec![exception_tag("IndexError")]);
    }

    #[test]
    fn a_base_matches_its_derived_types() {
        let tags = subtype_tags("LookupError");
        assert!(tags.contains(&exception_tag("LookupError")));
        assert!(tags.contains(&exception_tag("IndexError")));
        assert!(tags.contains(&exception_tag("KeyError")));
        assert_eq!(tags.len(), 3);
    }

    #[test]
    fn exception_excludes_the_baseexception_only_branch() {
        let tags = subtype_tags("Exception");
        assert!(tags.contains(&exception_tag("Exception")));
        assert!(tags.contains(&exception_tag("ZeroDivisionError")));
        assert!(!tags.contains(&exception_tag("GeneratorExit")));
        assert!(!tags.contains(&exception_tag("BaseException")));
    }

    #[test]
    fn baseexception_matches_everything() {
        assert_eq!(subtype_tags("BaseException").len(), EXCEPTION_HIERARCHY.len());
    }

    #[test]
    fn a_non_exception_name_has_no_tags() {
        assert!(subtype_tags("print").is_empty());
    }
}
