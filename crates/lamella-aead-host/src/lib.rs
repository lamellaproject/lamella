//! The host [`AeadBackend`]: RFC 5297 AES-SIV over the RustCrypto `aes-siv` crate --
//! AEAD_AES_SIV_CMAC_256 (a 32-byte key), the cipher RFC 8915 NTS mandates. Deterministic:
//! the same key + components + plaintext always seal to the same `V || C` bytes, which is why
//! the seam's two backends (this one and the device's mbedTLS-composed SIV) can be -- and are
//! -- pinned byte-identical by the RFC 5297 appendix vectors.

use aes_siv::KeyInit;
use aes_siv::siv::Aes128Siv;
use lamella_cil_runtime::aead::{AeadBackend, SIV_OVERHEAD};

/// The host AEAD engine. Stateless -- every call keys a fresh SIV instance, so there is
/// nothing to zeroize here beyond what `aes-siv` handles internally.
#[derive(Debug, Default)]
pub struct HostAead;

impl HostAead {
    /// A fresh engine.
    #[must_use]
    pub fn new() -> HostAead {
        HostAead
    }
}

impl AeadBackend for HostAead {
    fn siv_encrypt(
        &mut self,
        key: &[u8],
        components: &[&[u8]],
        plaintext: &[u8],
        out: &mut [u8],
    ) -> bool {
        if key.len() != 32 || out.len() != plaintext.len() + SIV_OVERHEAD {
            return false;
        }
        let mut siv = Aes128Siv::new(key.into());
        match siv.encrypt(components, plaintext) {
            Ok(sealed) if sealed.len() == out.len() => {
                out.copy_from_slice(&sealed);
                true
            }
            _ => false,
        }
    }

    fn siv_decrypt(
        &mut self,
        key: &[u8],
        components: &[&[u8]],
        sealed: &[u8],
        out: &mut [u8],
    ) -> bool {
        if key.len() != 32
            || sealed.len() < SIV_OVERHEAD
            || out.len() != sealed.len() - SIV_OVERHEAD
        {
            return false;
        }
        let mut siv = Aes128Siv::new(key.into());
        match siv.decrypt(components, sealed) {
            Ok(plaintext) if plaintext.len() == out.len() => {
                out.copy_from_slice(&plaintext);
                true
            }
            _ => false,
        }
    }
}
