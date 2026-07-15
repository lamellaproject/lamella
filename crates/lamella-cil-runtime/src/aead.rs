//! The AEAD crypto seam: RFC 5297 AES-SIV behind a trait the embedder supplies (host =
//! RustCrypto `aes-siv`; a device = an SIV composition over its mbedTLS AES primitive). The
//! authenticated-protocol tier drives it -- RFC 8915 NTS mandates AEAD_AES_SIV_CMAC_256 -- and
//! both backends must be BYTE-IDENTICAL (pinned by the RFC 5297 appendix vectors plus a
//! cross-backend differential), because SIV output is deterministic and lands on the wire.

use alloc::boxed::Box;

/// The SIV overhead: the 128-bit synthetic IV prepended to the ciphertext (RFC 5297's `V || C`
/// output shape).
pub const SIV_OVERHEAD: usize = 16;

/// The AEAD seam. Pure byte transforms -- no I/O, no blocking, no internal key storage.
pub trait AeadBackend: core::fmt::Debug {
    /// RFC 5297 SIV-ENCRYPT with AEAD_AES_SIV_CMAC_256 (a 32-byte key: the leftmost half keys
    /// S2V-CMAC, the rightmost half keys AES-CTR). `components` are the S2V associated-data
    /// components IN ORDER, before the plaintext (RFC 8915 NTS passes `[associated data,
    /// nonce]`). `out` receives `V(16) || C` and must be exactly `plaintext.len() + 16` bytes.
    /// Returns `false` on a bad key length or output size -- never a partial write.
    fn siv_encrypt(
        &mut self,
        key: &[u8],
        components: &[&[u8]],
        plaintext: &[u8],
        out: &mut [u8],
    ) -> bool;

    /// RFC 5297 SIV-DECRYPT: `sealed` is `V(16) || C`; `out` receives the plaintext and must be
    /// exactly `sealed.len() - 16` bytes. Returns `false` on an AUTHENTICATION FAILURE (or bad
    /// shapes) -- the caller must treat `out` as poisoned and discard it.
    fn siv_decrypt(
        &mut self,
        key: &[u8],
        components: &[&[u8]],
        sealed: &[u8],
        out: &mut [u8],
    ) -> bool;
}

/// A boxed AEAD backend, as the [`crate::interp::Vm`] stores it.
pub type BoxedAeadBackend = Box<dyn AeadBackend>;
