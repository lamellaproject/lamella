# VENDORED.md -- third-party components vendored into this repository

Every vendored component has an entry here: its upstream source and pinned version, how it was taken and verified,
what it is, its license, and any modification. Vendored files are never edited: local needs are met in Lamella's own
files instead. Changing a vendored file requires a new entry here.

## crates/lamella-tls-mbedtls/vendor/mbedtls/ (277 files) -- vendored 2026-07-09

- **Source:** Mbed TLS's official release archive for **v3.6.7**, the 3.6 long-term-support line:
  `https://github.com/Mbed-TLS/mbedtls/archive/refs/tags/v3.6.7.tar.gz`, SHA-256
  `7312b70b067b6a271961c8d36c3b8f9ba3e86fe6b26f18af13cd70430ee52ed1`.
- **Method:** extracted from that archive, keeping `include/`, `library/` and `LICENSE` only.
- **What it is:** the TLS implementation under Lamella's device TLS backend (`lamella-tls-mbedtls`).
- **License:** `Apache-2.0 OR GPL-2.0-or-later`, used under Apache-2.0. See its `LICENSE` and the notice in each file.
- **Modifications: NONE.**
