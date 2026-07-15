# lamella-tls-roots

The bundled system root-certificate store: the public web's Certificate Authorities, so a device
TLS client can validate an ordinary server's certificate chain (`VerifyMode::SystemRoots`) instead
of requiring a pinned certificate. A firmware that wants system-root trust links this crate and
registers its lookup with the device TLS backend (`lamella_tls_mbedtls::set_system_roots`).

## What it is, and the source

`src/roots.der` is the concatenated DER of every root in the **Mozilla CA certificate store**, and
`src/bundle.rs` is a subject-DN index over it (sorted, for a binary search). Both are **generated**
by `gen/gen_roots.py` from the official curl distribution of the Mozilla store:

- Upstream: <https://curl.se/ca/cacert.pem> (curl's `mk-ca-bundle.pl` extract of Mozilla NSS's
  `certdata.txt`). Fetched in-session; the `.pem` itself is **not committed** (`gen/.gitignore`) --
  only the generated `roots.der` + `bundle.rs` are, exactly as the Unicode tables are generated.
- The snapshot date is carried in the crate as `MOZILLA_SNAPSHOT` (from the bundle's own
  "Certificate data from Mozilla as of" line).

## Licensing

The Mozilla CA certificate store (NSS `certdata.txt`) is distributed under the **Mozilla Public
License 2.0** (MPL-2.0); curl redistributes it verbatim as `cacert.pem`. The individual root
certificates are the public self-signed certificates the CAs themselves publish for exactly this
use (embedding in trust stores). Redistributing the bundle -- as source (`roots.der`) or baked into
a firmware image -- is what the store exists for and is freely permitted; MPL-2.0's obligations
attach to modifications of the *curated file*, not to a program that merely consumes the certs. The
provenance is recorded here (this README + the generated-file header) so the attribution travels
with the data.

## Refresh cadence -- IMPORTANT for device longevity

A baked bundle is a **point-in-time snapshot** and the trust store is a living thing:

- Mozilla revises the store with roughly every Firefox release (about every 4-6 weeks); curl
  publishes a new `cacert.pem` on a similar cadence (a few times a quarter). Roots are **added**
  (a new CA), **removed / distrusted** (a CA that misbehaved), and every root also **expires** --
  several roots in any snapshot expire within a few years of it.
- Practical guidance for firmware authors:
  - **Refresh periodically.** Regenerate this crate (`python gen/gen_roots.py` after re-fetching
    `cacert.pem`) and reflash on a cadence you can sustain -- at minimum before a bundled root you
    rely on expires. `MOZILLA_SNAPSHOT` tells tooling how old the baked store is so a build/deploy
    step can warn past a chosen staleness threshold.
  - **Prefer pinning for long-lived, single-endpoint devices.** If a device only ever talks to one
    service you control, pin that service's certificate (`VerifyMode::PinnedCert`) rather than
    carrying the whole web's roots -- it never goes stale against a store you don't control, costs
    ~130 KB less flash, and is the tighter trust decision. System-roots is for a device that must
    reach arbitrary public servers.
  - A **clockless** board additionally cannot check the roots' validity windows; see the
    `surface.net.tls.clock` knob (skip-with-warning) -- but a real clock is the correct answer.

## RAM: the lazy lookup

The device mbedTLS pool is small (~32 KiB) -- far too little to parse ~120 roots at once. So the
device does not: `roots_for_issuer(issuer_dn)` returns only the root(s) whose subject matches a
peer chain's issuer, and mbedTLS's trusted-cert callback parses just those (usually one) per
handshake. The full store stays flash-resident DER; only the matching root is ever in RAM.

## Regenerate

```text
curl -sS -o gen/cacert.pem https://curl.se/ca/cacert.pem
python gen/gen_roots.py            # rewrites src/roots.der + src/bundle.rs
cargo test -p lamella-tls-roots
```
