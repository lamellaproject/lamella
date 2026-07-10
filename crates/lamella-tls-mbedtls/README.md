# lamella-tls-mbedtls

The DEVICE-side implementation of the interpreter's `TlsBackend` crypto seam
(`lamella_cil_runtime::tls`): a pure byte-transform TLS 1.2 client engine over a vendored
mbedTLS, for boards whose firmware runs TLS on the main MCU (the `managed` arm of the
`surface.net.tls` knob; WiFi modules with on-module TLS are the `module` arm).

## Vendored library

`vendor/mbedtls` is Mbed TLS **3.6.7** (the 3.6 LTS line), `include/` + `library/` +
`LICENSE` only, taken verbatim from the official release archive:

- source: `https://github.com/Mbed-TLS/mbedtls/archive/refs/tags/v3.6.7.tar.gz`
- archive SHA-256: `7312b70b067b6a271961c8d36c3b8f9ba3e86fe6b26f18af13cd70430ee52ed1`
- license: Apache-2.0 (see `vendor/mbedtls/LICENSE`)

The build profile lives in `csrc/lamella_mbedtls_config.h` (TLS 1.2 client, ECDHE/RSA +
AES-GCM + SHA-2, P-256/P-384, no filesystem, no wall clock -- certificate validity
windows are NOT checked on device until a wall-clock source lands). `csrc/lamella_tls_shim.c`
wraps the library in a handle-based opaque-pointer API so the Rust side needs no bindgen.

## Toolchain

Bare-metal targets need an ARM cross C compiler: `LAMELLA_ARM_GCC`, else
`arm-none-eabi-gcc` on PATH, else the MSYS2 default location
(`pacman -S mingw-w64-ucrt-x86_64-arm-none-eabi-gcc mingw-w64-ucrt-x86_64-arm-none-eabi-newlib`).
Host builds (the seam conformance tests) use the platform C compiler.

## Embedder contract

Register hardware entropy BEFORE creating sessions, then install the backend:

```rust,ignore
lamella_tls_mbedtls::set_entropy_source(trng_fill);
vm.set_tls_backend(Box::new(lamella_tls_mbedtls::MbedTlsDevice::new()));
```

Without an entropy source every configuration fails (no handshake with weak keys).
