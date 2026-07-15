//! The bundled system root-certificate store: the official curl/Mozilla CA bundle, indexed by
//! SUBJECT DN so a device TLS backend can find the root(s) that signed a peer's chain WITHOUT
//! parsing all ~120 roots into its small crypto pool. A firmware that wants system-root trust
//! (rather than a pinned certificate) links this crate and registers it with the backend.
#![no_std]

mod bundle;

use bundle::{ROOT_DER, ROOT_INDEX};

/// The upstream bundle's "Certificate data from Mozilla as of" snapshot (e.g. a UTC timestamp).
/// The Mozilla store AGES -- roots are added, distrusted, and expire -- so a baked bundle is a
/// point-in-time snapshot. A long-lived device should either refresh this crate periodically or
/// pin its own certificate; tooling can surface this to warn when the store is stale.
pub const MOZILLA_SNAPSHOT: &str = bundle::MOZILLA_SNAPSHOT;

/// The number of root certificates in the bundle.
#[must_use]
pub fn len() -> usize {
    ROOT_INDEX.len()
}

/// The DER of the root at `index` (`0..len()`), or `None` past the end -- for a consumer that
/// wants to walk the whole store (e.g. a count/verify pass at startup).
#[must_use]
pub fn root_der(index: usize) -> Option<&'static [u8]> {
    ROOT_INDEX.get(index).map(|&(_, offset, len)| {
        let start = offset as usize;
        &ROOT_DER[start..start + len as usize]
    })
}

/// The DER of each bundled root whose SUBJECT DN equals `issuer_dn` -- the roots that could have
/// signed a certificate bearing that issuer. `issuer_dn` is the raw DER of the child's issuer Name
/// (mbedTLS's `issuer_raw`), compared byte-for-byte against each root's subject Name (they are the
/// same bytes when a CA signs, so the common case matches exactly). The index is sorted by subject
/// DN, so this binary-searches the equal range: at most a handful of candidates, parsed on demand.
pub fn roots_for_issuer(issuer_dn: &[u8]) -> impl Iterator<Item = &'static [u8]> {
    let (lo, hi) = match ROOT_INDEX.binary_search_by(|&(subject, _, _)| subject.cmp(issuer_dn)) {
        Ok(hit) => {
            let mut lo = hit;
            while lo > 0 && ROOT_INDEX[lo - 1].0 == issuer_dn {
                lo -= 1;
            }
            let mut hi = hit + 1;
            while hi < ROOT_INDEX.len() && ROOT_INDEX[hi].0 == issuer_dn {
                hi += 1;
            }
            (lo, hi)
        }
        Err(_) => (0, 0),
    };
    ROOT_INDEX[lo..hi].iter().map(|&(_, offset, len)| {
        let start = offset as usize;
        &ROOT_DER[start..start + len as usize]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundle_is_populated_and_indexed() {
        assert!(len() >= 100, "the Mozilla bundle carries ~120 roots, got {}", len());
        for i in 0..len() {
            let der = root_der(i).expect("in range");
            assert!(!der.is_empty());
            assert_eq!(der[0], 0x30, "a certificate is a DER SEQUENCE");
        }
        for pair in ROOT_INDEX.windows(2) {
            assert!(pair[0].0 <= pair[1].0, "ROOT_INDEX must be sorted by subject DN");
        }
    }

    #[test]
    fn a_roots_own_subject_finds_itself() {
        let (subject, offset, len) = ROOT_INDEX[len_div(2)];
        let want = &ROOT_DER[offset as usize..offset as usize + len as usize];
        let found = roots_for_issuer(subject).any(|der| der == want);
        assert!(found, "a root's own subject DN must find its DER");
        assert_eq!(roots_for_issuer(b"\x30\x00").count(), 0);
    }

    fn len_div(d: usize) -> usize {
        super::len() / d
    }
}
