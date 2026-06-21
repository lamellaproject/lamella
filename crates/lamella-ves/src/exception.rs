//! The no-GC exception TAG model, the runtime half of a decentralized contract shared with
//! the AOT backend and the compiler. An exception type is identified everywhere -- throw site,
//! every catch, the runtime, and the AOT image -- by one `u32` tag, so a thrown exception can
//! cross the AOT <-> interpreter boundary in mixed-mode execution and both tiers agree on which
//! catch handles it.

/// The FNV-1a 32-bit offset basis (2166136261), the seed of the running hash.
const FNV_OFFSET_BASIS: u32 = 0x811c_9dc5;
/// The FNV-1a 32-bit prime (16777619), the per-byte multiplier.
const FNV_PRIME: u32 = 0x0100_0193;
/// The high bit forced onto every tag: it makes the tag nonzero (a "failure" value, so `0` can
/// mean "no exception in flight") and never collides with that sentinel.
const TAG_HIGH_BIT: u32 = 0x8000_0000;

/// FNV-1a 32-bit, folding `bytes` into the running `hash` (XOR the byte, then multiply by the
/// prime). Identical to `lamella_metadata`'s private `fnv1a32`, so the two crates hash byte-for-byte
/// the same way without sharing code.
fn fnv1a32(hash: u32, bytes: &[u8]) -> u32 {
    let mut hash = hash;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The exception TAG of a type named by its full name (`"System.InvalidOperationException"`,
/// `"MyError"`): FNV-1a-32 over the UTF-8 bytes of the name with the high bit forced, so it is a
/// nonzero value identical wherever the type is named.
///
/// This MUST be byte-for-byte identical to `lamella_metadata::Assembly::exception_tag`, which hashes
/// `namespace`, then `"."`, then `name` when the namespace is non-empty, else just `name`. Passing
/// the already-joined `"namespace.name"` here produces the same bytes in the same order, hence the
/// same tag. (A type in the global namespace -- no `.` -- is hashed as the bare name on both sides.)
#[must_use]
pub fn exception_tag(full_name: &str) -> u32 {
    fnv1a32(FNV_OFFSET_BASIS, full_name.as_bytes()) | TAG_HIGH_BIT
}

/// Whether a `catch` whose type has tag `catch_tag` catches an exception whose base-chain tag
/// vector is `thrown_chain` (leaf-first, up to and including `System.Exception`). The catch matches
/// when `catch_tag` is a MEMBER of the vector -- the cold-path scan the compiler specified, which
/// gives the same verdict as walking the thrown type's live base chain.
///
/// The universal-catch convention (`catch (System.Exception)` / `catch (System.Object)` / a
/// typeless `catch {}`, which match any in-flight exception) needs no special case: every
/// exception's vector ends in `tag(System.Exception)`, so a catch on the exception root is a member
/// of every chain. A non-exception or absent catch tag (`0`) never appears in a chain, so it matches
/// nothing. Use [`tag_is_exact`] for `==` (no-subtyping) matching.
#[must_use]
pub fn tag_is_subtype(catch_tag: u32, thrown_chain: &[u32]) -> bool {
    thrown_chain.contains(&catch_tag)
}

/// Whether `catch_tag` matches the thrown exception EXACTLY -- equal to the leaf (`thrown_chain[0]`,
/// the thrown type's own tag), no subtyping. This is the compiler's current `DispatchMatch::ExactTag`
/// rule; [`tag_is_subtype`] is the membership (full subtyping) form that supersedes it.
#[must_use]
pub fn tag_is_exact(catch_tag: u32, thrown_chain: &[u32]) -> bool {
    thrown_chain.first() == Some(&catch_tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::string::String;

    /// The compiler's emitter (`lamella_metadata::reader::fnv1a32` + `exception_tag`), reproduced
    /// from its source constants so this crate -- which does NOT depend on `lamella-metadata` -- can
    /// assert byte-for-byte equality without a dependency cycle. The end-to-end cross-check against
    /// the REAL `Assembly::exception_tag` over a compiled assembly lives in `lamella-load`'s
    /// `exception_tag_matches_compiler` test (that crate reaches both).
    fn compiler_exception_tag(namespace: &str, name: &str) -> u32 {
        let mut hash = 0x811c_9dc5u32;
        let fold = |mut hash: u32, bytes: &[u8]| {
            for byte in bytes {
                hash ^= u32::from(*byte);
                hash = hash.wrapping_mul(0x0100_0193);
            }
            hash
        };
        if !namespace.is_empty() {
            hash = fold(hash, namespace.as_bytes());
            hash = fold(hash, b".");
        }
        hash = fold(hash, name.as_bytes());
        hash | 0x8000_0000
    }

    /// The full name the interpreter passes to [`exception_tag`] -- `namespace.name`, or the bare
    /// `name` in the global namespace -- producing the same bytes the compiler hashes piecewise.
    fn full_name(namespace: &str, name: &str) -> String {
        if namespace.is_empty() {
            name.into()
        } else {
            format!("{namespace}.{name}")
        }
    }

    #[test]
    fn tag_matches_compiler_emitter() {
        for (namespace, name) in [
            ("System", "Exception"),
            ("System", "SystemException"),
            ("System", "InvalidOperationException"),
            ("System", "ArgumentNullException"),
            ("System", "DivideByZeroException"),
            ("", "MyError"),
        ] {
            assert_eq!(
                exception_tag(&full_name(namespace, name)),
                compiler_exception_tag(namespace, name),
                "tag for {namespace:?}.{name:?} must match the compiler emitter",
            );
        }
    }

    #[test]
    fn tag_is_nonzero_with_high_bit() {
        for name in ["System.Exception", "MyError", ""] {
            let tag = exception_tag(name);
            assert_ne!(tag, 0, "a tag is never the no-exception sentinel");
            assert_eq!(tag & 0x8000_0000, 0x8000_0000, "the high bit is set");
        }
    }

    #[test]
    fn pinned_literal_tags_lock_the_contract() {
        assert_eq!(exception_tag("System.Exception"), 0xAF8E_039F);
        assert_eq!(
            exception_tag("System.InvalidOperationException"),
            0xA43E_7B3F
        );
    }

    #[test]
    fn membership_is_subtyping() {
        let exc = exception_tag("System.Exception");
        let sys = exception_tag("System.SystemException");
        let ioe = exception_tag("System.InvalidOperationException");
        let mine = exception_tag("MyError");
        let chain = [mine, ioe, sys, exc];

        assert!(tag_is_subtype(mine, &chain));
        assert!(tag_is_subtype(ioe, &chain));
        assert!(tag_is_subtype(sys, &chain));
        assert!(tag_is_subtype(exc, &chain), "the root catch is universal");

        let unrelated = exception_tag("System.FormatException");
        assert!(!tag_is_subtype(unrelated, &chain));

        assert!(tag_is_exact(mine, &chain));
        assert!(!tag_is_exact(ioe, &chain));
    }

    #[test]
    fn catch_all_matches_any_via_root_membership() {
        let exc = exception_tag("System.Exception");
        for leaf in ["System.FormatException", "System.OutOfMemoryException", "MyError"] {
            let chain = [exception_tag(leaf), exc];
            assert!(tag_is_subtype(exc, &chain));
        }
        assert!(!tag_is_subtype(0, &[exc]));
    }
}
