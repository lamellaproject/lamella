//! The DEVICE [`TlsBackend`]: the interpreter's TLS crypto seam over a vendored,
//! size-trimmed mbedTLS (TLS 1.2 client -- see `csrc/lamella_mbedtls_config.h` for the
//! exact profile). Like every implementor of the seam it is a PURE byte transform: the
//! engine holds no socket and never blocks; the managed `SslStream` does all socket I/O
//! and pumps it (`process` / `wants_write`+`write_tls` / `read_tls` /
//! `read_plain`+`write_plain`). Ciphertext moves through in-memory queues that mbedTLS
//! reads and writes via its BIO callbacks, so `MBEDTLS_ERR_SSL_WANT_READ` becomes the
//! seam's "feed me more" -- green-thread parking stays entirely in the managed layer.

#![no_std]
#![allow(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, c_void};
use core::sync::atomic::{AtomicPtr, Ordering};

use lamella_cil_runtime::tls::{
    TlsBackend, TlsConfigHandle, TlsHandle, TlsStack, TlsState, VerifyMode,
};

unsafe extern "C" {
    fn lam_tls_client_new(
        ca_pem: *const u8,
        ca_len: usize,
        hostname: *const c_char,
        verify_mode: c_int,
        user: *mut c_void,
    ) -> *mut c_void;
    fn lam_tls_handshake(session: *mut c_void) -> c_int;
    fn lam_tls_read(session: *mut c_void, buf: *mut u8, len: usize) -> c_int;
    fn lam_tls_write(session: *mut c_void, buf: *const u8, len: usize) -> c_int;
    fn lam_tls_peer_cert(session: *mut c_void, out: *mut u8, out_len: usize) -> c_int;
    fn lam_tls_close(session: *mut c_void);
}

const SHIM_WANT: c_int = -1;
const SHIM_CLOSED: c_int = -2;

/// The registered hardware entropy source (`None` until the embedder provides one).
/// Stored as a raw fn pointer so registration works from a bare-metal boot path.
static ENTROPY_SOURCE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Registers the entropy source every session's CTR_DRBG seeds from. The function fills
/// the buffer with hardware randomness and returns `true`; returning `false` fails the
/// consuming operation loudly. A device points this at its TRNG; host conformance tests
/// supply a process-local source.
pub fn set_entropy_source(source: fn(&mut [u8]) -> bool) {
    ENTROPY_SOURCE.store(source as *mut (), Ordering::Release);
}

/// C hook (`csrc/lamella_tls_shim.c` -> `mbedtls_hardware_poll`): fills `output` from the
/// registered source; nonzero = failure (no source registered, or the source failed).
///
/// Safety: called by mbedTLS with a valid `output`/`len` pair describing writable memory.
#[unsafe(no_mangle)]
extern "C" fn lamella_entropy_poll(output: *mut u8, len: usize) -> c_int {
    let source = ENTROPY_SOURCE.load(Ordering::Acquire);
    if source.is_null() || output.is_null() {
        return 1;
    }
    let source: fn(&mut [u8]) -> bool = unsafe { core::mem::transmute(source) };
    let buffer = unsafe { core::slice::from_raw_parts_mut(output, len) };
    if source(buffer) { 0 } else { 1 }
}

/// Bytes of allocation header holding the payload size (16 keeps the payload the same
/// alignment `Layout` grants the block).
const ALLOC_HEADER: usize = 16;

/// C hook: `calloc` for mbedTLS, backed by the global allocator with a size header so
/// [`lamella_mbedtls_free`] can reconstruct the layout. Zeroed per calloc's contract.
///
/// Safety: pure allocator; returns null on overflow or exhaustion (mbedTLS handles null).
#[unsafe(no_mangle)]
extern "C" fn lamella_mbedtls_calloc(count: usize, size: usize) -> *mut c_void {
    let Some(payload) = count.checked_mul(size) else {
        return core::ptr::null_mut();
    };
    let Some(total) = payload.checked_add(ALLOC_HEADER) else {
        return core::ptr::null_mut();
    };
    let Ok(layout) = core::alloc::Layout::from_size_align(total, ALLOC_HEADER) else {
        return core::ptr::null_mut();
    };
    let block = unsafe { alloc::alloc::alloc_zeroed(layout) };
    if block.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        block.cast::<usize>().write(payload);
        block.add(ALLOC_HEADER).cast()
    }
}

/// C hook: `free` for [`lamella_mbedtls_calloc`] blocks.
///
/// Safety: `pointer` is null or a payload pointer this module's calloc returned.
#[unsafe(no_mangle)]
extern "C" fn lamella_mbedtls_free(pointer: *mut c_void) {
    if pointer.is_null() {
        return;
    }
    unsafe {
        let block = pointer.cast::<u8>().sub(ALLOC_HEADER);
        let payload = block.cast::<usize>().read();
        let layout =
            core::alloc::Layout::from_size_align_unchecked(payload + ALLOC_HEADER, ALLOC_HEADER);
        alloc::alloc::dealloc(block, layout);
    }
}

/// The ciphertext queues one session shares with mbedTLS's BIO callbacks. Boxed to a
/// stable address (the C side holds the pointer for the session's whole life).
struct BioState {
    /// Ciphertext from the socket: `read_tls` fills, the BIO recv callback drains.
    incoming: VecDeque<u8>,
    /// Ciphertext to the socket: the BIO send callback fills, `write_tls` drains.
    outgoing: VecDeque<u8>,
}

/// C hook (BIO send): queue outgoing ciphertext. Accepts everything offered.
///
/// Safety: `user` is the live `BioState` this module handed to `lam_tls_client_new`;
/// `buf`/`len` describe readable memory owned by mbedTLS for the call.
#[unsafe(no_mangle)]
extern "C" fn lamella_bio_send(user: *mut c_void, buf: *const u8, len: usize) -> c_int {
    let bio = unsafe { &mut *user.cast::<BioState>() };
    let bytes = unsafe { core::slice::from_raw_parts(buf, len) };
    bio.outgoing.extend(bytes.iter().copied());
    len.min(c_int::MAX as usize) as c_int
}

/// C hook (BIO recv): dequeue incoming ciphertext; `0` = nothing queued (the shim maps it
/// to `MBEDTLS_ERR_SSL_WANT_READ`).
///
/// Safety: as [`lamella_bio_send`], with `buf` writable.
#[unsafe(no_mangle)]
extern "C" fn lamella_bio_recv(user: *mut c_void, buf: *mut u8, len: usize) -> c_int {
    let bio = unsafe { &mut *user.cast::<BioState>() };
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    let take = out.len().min(bio.incoming.len());
    for slot in out.iter_mut().take(take) {
        *slot = bio.incoming.pop_front().expect("length checked");
    }
    take.min(c_int::MAX as usize) as c_int
}

/// A prepared client configuration: the trust decision plus (for pinned mode) the root
/// bundle, applied when a session is created.
struct StoredConfig {
    verify: VerifyMode,
    /// PEM roots, NUL-terminated the way mbedTLS's PEM parser expects.
    roots: Option<Vec<u8>>,
}

/// A live session: the C-side handle plus the shared BIO queues and the seam state.
struct Session {
    shim: *mut c_void,
    bio: *mut BioState,
    established: bool,
    closed: bool,
    failed: bool,
}

/// The device TLS engine. `configs`/`sessions` are append-only tables; a handle is an
/// index -- the same shape as the host backend, so the managed pump cannot tell them apart.
#[derive(Default)]
pub struct MbedTlsDevice {
    configs: Vec<StoredConfig>,
    sessions: Vec<Option<Session>>,
}

impl core::fmt::Debug for MbedTlsDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MbedTlsDevice")
            .field("configs", &self.configs.len())
            .field("sessions", &self.sessions.len())
            .finish()
    }
}

impl MbedTlsDevice {
    /// A fresh engine. The embedder must have registered an entropy source
    /// ([`set_entropy_source`]) before the first session is created.
    #[must_use]
    pub fn new() -> MbedTlsDevice {
        MbedTlsDevice::default()
    }

    fn session_mut(&mut self, tls: TlsHandle) -> Option<&mut Session> {
        self.sessions.get_mut(tls as usize).and_then(Option::as_mut)
    }
}

impl Drop for MbedTlsDevice {
    fn drop(&mut self) {
        for index in 0..self.sessions.len() {
            self.close(index as TlsHandle);
        }
    }
}

impl TlsBackend for MbedTlsDevice {
    /// The single compiled-in stack: mbedTLS.
    fn default_stack(&self) -> i32 {
        1
    }

    fn client_config(
        &mut self,
        _stack: TlsStack,
        verify: VerifyMode,
        roots_pem: Option<&[u8]>,
    ) -> Option<TlsConfigHandle> {
        if ENTROPY_SOURCE.load(Ordering::Acquire).is_null() {
            return None;
        }
        let roots = match verify {
            VerifyMode::SystemRoots => return None,
            VerifyMode::PinnedCert => {
                let pem = roots_pem?;
                if pem.is_empty() {
                    return None;
                }
                let mut bundle = pem.to_vec();
                bundle.push(0);
                Some(bundle)
            }
            VerifyMode::AcceptAny => None,
        };
        self.configs.push(StoredConfig { verify, roots });
        Some((self.configs.len() - 1) as TlsConfigHandle)
    }

    /// The device rung is client-only (the NETMF-parity direction); serving TLS from the
    /// board is a later tier.
    fn server_config(
        &mut self,
        _stack: TlsStack,
        _identity_pfx: &[u8],
        _password: &str,
    ) -> Option<TlsConfigHandle> {
        None
    }

    fn client_new(&mut self, config: TlsConfigHandle, hostname: &str) -> Option<TlsHandle> {
        let stored = self.configs.get(config as usize)?;
        if hostname.as_bytes().contains(&0) {
            return None;
        }
        let mut hostname_z = Vec::with_capacity(hostname.len() + 1);
        hostname_z.extend_from_slice(hostname.as_bytes());
        hostname_z.push(0);

        let verify_mode: c_int = match stored.verify {
            VerifyMode::PinnedCert => 0,
            VerifyMode::AcceptAny => 1,
            VerifyMode::SystemRoots => return None,
        };
        let (ca_ptr, ca_len) = match &stored.roots {
            Some(bundle) => (bundle.as_ptr(), bundle.len()),
            None => (core::ptr::null(), 0),
        };

        let bio = Box::into_raw(Box::new(BioState {
            incoming: VecDeque::new(),
            outgoing: VecDeque::new(),
        }));
        let shim = unsafe {
            lam_tls_client_new(
                ca_ptr,
                ca_len,
                hostname_z.as_ptr().cast::<c_char>(),
                verify_mode,
                bio.cast::<c_void>(),
            )
        };
        if shim.is_null() {
            drop(unsafe { Box::from_raw(bio) });
            return None;
        }
        self.sessions.push(Some(Session {
            shim,
            bio,
            established: false,
            closed: false,
            failed: false,
        }));
        Some((self.sessions.len() - 1) as TlsHandle)
    }

    fn server_new(&mut self, _config: TlsConfigHandle) -> Option<TlsHandle> {
        None
    }

    fn process(&mut self, tls: TlsHandle) -> TlsState {
        let Some(session) = self.session_mut(tls) else {
            return TlsState::Error;
        };
        if session.failed {
            return TlsState::Error;
        }
        if !session.established {
            match unsafe { lam_tls_handshake(session.shim) } {
                1 => session.established = true,
                0 => return TlsState::Handshaking,
                _ => {
                    session.failed = true;
                    return TlsState::Error;
                }
            }
        }
        if session.closed {
            TlsState::Closed
        } else {
            TlsState::Established
        }
    }

    fn wants_write(&mut self, tls: TlsHandle) -> bool {
        match self.session_mut(tls) {
            Some(session) => unsafe { !(*session.bio).outgoing.is_empty() },
            None => false,
        }
    }

    fn write_tls(&mut self, tls: TlsHandle, out: &mut [u8]) -> usize {
        let Some(session) = self.session_mut(tls) else {
            return 0;
        };
        let bio = unsafe { &mut *session.bio };
        let take = out.len().min(bio.outgoing.len());
        for slot in out.iter_mut().take(take) {
            *slot = bio.outgoing.pop_front().expect("length checked");
        }
        take
    }

    fn read_tls(&mut self, tls: TlsHandle, input: &[u8]) -> usize {
        let Some(session) = self.session_mut(tls) else {
            return 0;
        };
        let bio = unsafe { &mut *session.bio };
        bio.incoming.extend(input.iter().copied());
        input.len()
    }

    fn read_plain(&mut self, tls: TlsHandle, out: &mut [u8]) -> Option<usize> {
        let session = self.session_mut(tls)?;
        if session.failed || session.closed {
            return None;
        }
        if out.is_empty() {
            return Some(0);
        }
        let rc = unsafe { lam_tls_read(session.shim, out.as_mut_ptr(), out.len()) };
        match rc {
            n if n >= 0 => Some(n as usize),
            SHIM_WANT => Some(0),
            SHIM_CLOSED => {
                session.closed = true;
                None
            }
            _ => {
                session.failed = true;
                None
            }
        }
    }

    fn write_plain(&mut self, tls: TlsHandle, input: &[u8]) -> usize {
        let Some(session) = self.session_mut(tls) else {
            return 0;
        };
        if session.failed || input.is_empty() {
            return 0;
        }
        let rc = unsafe { lam_tls_write(session.shim, input.as_ptr(), input.len()) };
        match rc {
            n if n >= 0 => n as usize,
            SHIM_WANT => 0,
            _ => {
                session.failed = true;
                0
            }
        }
    }

    fn peer_cert(&mut self, tls: TlsHandle, out: &mut [u8]) -> usize {
        let Some(session) = self.session_mut(tls) else {
            return 0;
        };
        let len = unsafe { lam_tls_peer_cert(session.shim, out.as_mut_ptr(), out.len()) };
        len.max(0) as usize
    }

    fn close(&mut self, tls: TlsHandle) {
        let Some(slot) = self.sessions.get_mut(tls as usize) else {
            return;
        };
        if let Some(session) = slot.take() {
            unsafe {
                lam_tls_close(session.shim);
                drop(Box::from_raw(session.bio));
            }
        }
    }
}
