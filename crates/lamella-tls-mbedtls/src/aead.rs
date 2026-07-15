//! The device [`AeadBackend`]: RFC 5297 AES-SIV (AEAD_AES_SIV_CMAC_256) COMPOSED over the
//! vendored mbedTLS AES-128 encrypt-block primitive -- CMAC (NIST SP 800-38B), S2V, and CTR
//! are built here in Rust, so the device needs only `MBEDTLS_AES_C` (no cipher layer, no CMAC
//! module). The composition is deterministic and BYTE-IDENTICAL to the host backend
//! (RustCrypto `aes-siv`): the RFC 5297 appendix vectors plus a cross-backend differential pin
//! it, because SIV output goes on the wire (RFC 8915 NTS).

use core::ffi::c_void;

use lamella_cil_runtime::aead::{AeadBackend, SIV_OVERHEAD};

unsafe extern "C" {
    fn lam_aes128_new(key: *const u8) -> *mut c_void;
    fn lam_aes128_encrypt_block(ctx: *mut c_void, input: *const u8, output: *mut u8);
    fn lam_aes128_free(ctx: *mut c_void);
}

/// One keyed AES-128 ENCRYPT-block primitive (an mbedTLS context behind the shim).
struct Aes128 {
    ctx: *mut c_void,
}

impl Aes128 {
    fn new(key: &[u8; 16]) -> Option<Aes128> {
        let ctx = unsafe { lam_aes128_new(key.as_ptr()) };
        if ctx.is_null() { None } else { Some(Aes128 { ctx }) }
    }

    fn encrypt_block(&self, block: [u8; 16]) -> [u8; 16] {
        let mut out = [0u8; 16];
        unsafe { lam_aes128_encrypt_block(self.ctx, block.as_ptr(), out.as_mut_ptr()) };
        out
    }
}

impl Drop for Aes128 {
    fn drop(&mut self) {
        unsafe { lam_aes128_free(self.ctx) };
    }
}

/// RFC 5297's `dbl`: a left shift of the 128-bit block, xor 0x87 into the low byte when the
/// shifted-off bit was one (GF(2^128) doubling -- also SP 800-38B's subkey step).
fn dbl(block: [u8; 16]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let mut carry = 0u8;
    for index in (0..16).rev() {
        out[index] = (block[index] << 1) | carry;
        carry = block[index] >> 7;
    }
    if carry != 0 {
        out[15] ^= 0x87;
    }
    out
}

fn xor(a: [u8; 16], b: [u8; 16]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for index in 0..16 {
        out[index] = a[index] ^ b[index];
    }
    out
}

/// NIST SP 800-38B AES-CMAC over the block primitive: subkeys from `dbl(E(0))`, CBC-MAC with
/// the final block xor'd with K1 (complete) or padded-and-xor'd with K2 (incomplete/empty).
struct Cmac<'aes> {
    aes: &'aes Aes128,
    k1: [u8; 16],
    k2: [u8; 16],
}

impl<'aes> Cmac<'aes> {
    fn new(aes: &'aes Aes128) -> Cmac<'aes> {
        let k1 = dbl(aes.encrypt_block([0u8; 16]));
        let k2 = dbl(k1);
        Cmac { aes, k1, k2 }
    }

    fn mac(&self, message: &[u8]) -> [u8; 16] {
        let mut state = [0u8; 16];
        if message.is_empty() {
            let mut last = [0u8; 16];
            last[0] = 0x80;
            return self.aes.encrypt_block(xor(last, self.k2));
        }
        let full_blocks = (message.len() - 1) / 16;
        for block_index in 0..full_blocks {
            let mut block = [0u8; 16];
            block.copy_from_slice(&message[block_index * 16..block_index * 16 + 16]);
            state = self.aes.encrypt_block(xor(state, block));
        }
        let tail = &message[full_blocks * 16..];
        let mut last = [0u8; 16];
        last[..tail.len()].copy_from_slice(tail);
        let last = if tail.len() == 16 {
            xor(last, self.k1)
        } else {
            last[tail.len()] = 0x80;
            xor(last, self.k2)
        };
        self.aes.encrypt_block(xor(state, last))
    }
}

/// RFC 5297 S2V over CMAC: the associated-data components fold into `D` by
/// `dbl(D) xor CMAC(S)`; the final (plaintext) component enters by `xorend` when it is at
/// least one block, else by `dbl + pad`.
fn s2v(cmac: &Cmac<'_>, components: &[&[u8]], plaintext: &[u8]) -> [u8; 16] {
    let mut d = cmac.mac(&[0u8; 16]);
    for component in components {
        d = xor(dbl(d), cmac.mac(component));
    }
    if plaintext.len() >= 16 {
        let mut mixed = alloc::vec::Vec::with_capacity(plaintext.len());
        mixed.extend_from_slice(plaintext);
        let split = plaintext.len() - 16;
        for index in 0..16 {
            mixed[split + index] ^= d[index];
        }
        cmac.mac(&mixed)
    } else {
        let mut padded = [0u8; 16];
        padded[..plaintext.len()].copy_from_slice(plaintext);
        padded[plaintext.len()] = 0x80;
        cmac.mac(&xor(dbl(d), padded))
    }
}

/// RFC 5297 CTR: the IV is `V` with bits 31 and 63 cleared; the counter is the whole 128-bit
/// big-endian block, incremented per keystream block.
fn ctr_transform(aes: &Aes128, v: &[u8; 16], data: &mut [u8]) {
    let mut counter = *v;
    counter[8] &= 0x7f;
    counter[12] &= 0x7f;
    let mut offset = 0;
    while offset < data.len() {
        let keystream = aes.encrypt_block(counter);
        let take = (data.len() - offset).min(16);
        for index in 0..take {
            data[offset + index] ^= keystream[index];
        }
        offset += take;
        for byte in counter.iter_mut().rev() {
            *byte = byte.wrapping_add(1);
            if *byte != 0 {
                break;
            }
        }
    }
}

/// Splits the AEAD_AES_SIV_CMAC_256 key: leftmost half keys S2V-CMAC, rightmost keys CTR.
fn split_key(key: &[u8]) -> Option<([u8; 16], [u8; 16])> {
    if key.len() != 32 {
        return None;
    }
    let mut k1 = [0u8; 16];
    let mut k2 = [0u8; 16];
    k1.copy_from_slice(&key[..16]);
    k2.copy_from_slice(&key[16..]);
    Some((k1, k2))
}

/// The device AEAD engine. Stateless -- every call keys fresh block contexts.
#[derive(Debug, Default)]
pub struct MbedAead;

impl MbedAead {
    /// A fresh engine.
    #[must_use]
    pub fn new() -> MbedAead {
        MbedAead
    }
}

impl AeadBackend for MbedAead {
    fn siv_encrypt(
        &mut self,
        key: &[u8],
        components: &[&[u8]],
        plaintext: &[u8],
        out: &mut [u8],
    ) -> bool {
        let Some((mac_key, ctr_key)) = split_key(key) else {
            return false;
        };
        if out.len() != plaintext.len() + SIV_OVERHEAD {
            return false;
        }
        let (Some(mac_aes), Some(ctr_aes)) = (Aes128::new(&mac_key), Aes128::new(&ctr_key))
        else {
            return false;
        };
        let cmac = Cmac::new(&mac_aes);
        let v = s2v(&cmac, components, plaintext);
        out[..16].copy_from_slice(&v);
        out[16..].copy_from_slice(plaintext);
        ctr_transform(&ctr_aes, &v, &mut out[16..]);
        true
    }

    fn siv_decrypt(
        &mut self,
        key: &[u8],
        components: &[&[u8]],
        sealed: &[u8],
        out: &mut [u8],
    ) -> bool {
        let Some((mac_key, ctr_key)) = split_key(key) else {
            return false;
        };
        if sealed.len() < SIV_OVERHEAD || out.len() != sealed.len() - SIV_OVERHEAD {
            return false;
        }
        let (Some(mac_aes), Some(ctr_aes)) = (Aes128::new(&mac_key), Aes128::new(&ctr_key))
        else {
            return false;
        };
        let mut v = [0u8; 16];
        v.copy_from_slice(&sealed[..16]);
        out.copy_from_slice(&sealed[16..]);
        ctr_transform(&ctr_aes, &v, out);
        let cmac = Cmac::new(&mac_aes);
        let expected = s2v(&cmac, components, out);
        let mut difference = 0u8;
        for index in 0..16 {
            difference |= expected[index] ^ v[index];
        }
        if difference != 0 {
            out.iter_mut().for_each(|byte| *byte = 0);
            return false;
        }
        true
    }
}
