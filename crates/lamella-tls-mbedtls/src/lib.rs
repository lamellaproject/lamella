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

/// The registered wall-clock source (`None` until the embedder provides one). Stored as a
/// raw fn pointer so registration works from a bare-metal boot path, like the entropy source.
static TIME_SOURCE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Registers the wall clock certificate validity WINDOWS are checked against (the current
/// time in Unix seconds). Without one the clock reads 0 (the epoch): every real certificate
/// then pre-dates "now", so a REQUIRED (pinned-cert / system-root) verify fails "not yet
/// valid" LOUDLY rather than trusting an unchecked window -- fail closed. A device points
/// this at an RTC, an SNTP-synced counter, or a baked build-time floor; the accept-any
/// bench path (VERIFY_NONE) never consults it. Optional: a client that only uses accept-any
/// need not register a clock.
pub fn set_time_source(source: fn() -> u64) {
    TIME_SOURCE.store(source as *mut (), Ordering::Release);
}

/// C hook (`csrc/lamella_tls_shim.c` -> `lamella_mbedtls_time`): the current time in Unix
/// seconds, or 0 when no source is registered.
#[unsafe(no_mangle)]
extern "C" fn lamella_tls_time() -> i64 {
    let source = TIME_SOURCE.load(Ordering::Acquire);
    if source.is_null() {
        return 0;
    }
    let source: fn() -> u64 = unsafe { core::mem::transmute(source) };
    source() as i64
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


/// The pool size. On a device: one session's record buffers (16 KiB in + 4 KiB out) +
/// ssl/x509 state + bignum churn peak, with working headroom -- an embedder wanting
/// concurrent sessions grows this and its RAM budget together. On a HOST the pool is
/// roomier: the conformance tests run sessions in parallel test threads, and host RAM is
/// not the scarce resource the pool exists to discipline.
const POOL_BYTES: usize = if cfg!(target_os = "none") { 32 * 1024 } else { 128 * 1024 };

/// Block granularity and payload alignment: every block size is a multiple of this, and
/// headers are one unit so payloads stay aligned.
const ALLOC_UNIT: usize = 16;

#[allow(dead_code)]
#[repr(align(16))]
struct Pool([u8; POOL_BYTES]);

static mut POOL: Pool = Pool([0; POOL_BYTES]);

/// A free block's header: its total size (header included) and the next free block by
/// ascending address (address order is what makes coalescing a neighbor check).
#[repr(C)]
struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

/// The free-list head. Null until [`pool_init`] links the whole pool as one block;
/// `FREE_INIT` marks initialization done even when the list is momentarily empty (fully
/// allocated).
static mut FREE_HEAD: *mut FreeBlock = core::ptr::null_mut();
static mut FREE_INIT: bool = false;

/// The pool's spin guard. A device pumps one session on one thread (uncontended, so this
/// costs one uncontended CAS per call); the HOST conformance tests run in cargo's
/// parallel test threads and genuinely contend.
static POOL_LOCK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Runs `body` holding the pool lock.
fn with_pool<T>(body: impl FnOnce() -> T) -> T {
    while POOL_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    let result = body();
    POOL_LOCK.store(false, Ordering::Release);
    result
}

/// Lazily links the whole pool as a single free block.
///
/// Safety: single-threaded contract; called only from the allocator entry points.
unsafe fn pool_init() {
    unsafe {
        if *core::ptr::addr_of!(FREE_INIT) {
            return;
        }
        let head = core::ptr::addr_of_mut!(POOL).cast::<FreeBlock>();
        (*head).size = POOL_BYTES;
        (*head).next = core::ptr::null_mut();
        *core::ptr::addr_of_mut!(FREE_HEAD) = head;
        *core::ptr::addr_of_mut!(FREE_INIT) = true;
    }
}

/// C hook: `calloc` for mbedTLS over the pool -- first fit, split when the remainder can
/// hold a block, zeroed per calloc's contract. An allocated block's header keeps its size
/// for [`lamella_mbedtls_free`].
///
/// Safety: pure allocator; returns null on overflow or exhaustion (mbedTLS handles null).
#[unsafe(no_mangle)]
extern "C" fn lamella_mbedtls_calloc(count: usize, size: usize) -> *mut c_void {
    let Some(payload) = count.checked_mul(size) else {
        return core::ptr::null_mut();
    };
    let Some(raw) = payload.checked_add(ALLOC_UNIT) else {
        return core::ptr::null_mut();
    };
    let needed = (raw + ALLOC_UNIT - 1) & !(ALLOC_UNIT - 1);
    with_pool(|| unsafe {
        pool_init();
        let mut prev: *mut FreeBlock = core::ptr::null_mut();
        let mut current = *core::ptr::addr_of!(FREE_HEAD);
        while !current.is_null() {
            if (*current).size >= needed {
                let remainder = (*current).size - needed;
                let successor = if remainder >= ALLOC_UNIT * 2 {
                    let tail = current.cast::<u8>().add(needed).cast::<FreeBlock>();
                    (*tail).size = remainder;
                    (*tail).next = (*current).next;
                    (*current).size = needed;
                    tail
                } else {
                    (*current).next
                };
                if prev.is_null() {
                    *core::ptr::addr_of_mut!(FREE_HEAD) = successor;
                } else {
                    (*prev).next = successor;
                }
                let block = current.cast::<u8>();
                block.cast::<usize>().write((*current).size);
                let user = block.add(ALLOC_UNIT);
                core::ptr::write_bytes(user, 0, payload);
                return user.cast();
            }
            prev = current;
            current = (*current).next;
        }
        core::ptr::null_mut()
    })
}

/// C hook: `free` for [`lamella_mbedtls_calloc`] blocks -- inserts by address and
/// coalesces with adjacent free neighbors, so churn cannot fragment the pool away.
///
/// Safety: `pointer` is null or a payload pointer this module's calloc returned.
#[unsafe(no_mangle)]
extern "C" fn lamella_mbedtls_free(pointer: *mut c_void) {
    if pointer.is_null() {
        return;
    }
    with_pool(|| unsafe {
        let block = pointer.cast::<u8>().sub(ALLOC_UNIT).cast::<FreeBlock>();
        let size = block.cast::<usize>().read();
        (*block).size = size;
        let mut prev: *mut FreeBlock = core::ptr::null_mut();
        let mut current = *core::ptr::addr_of!(FREE_HEAD);
        while !current.is_null() && current < block {
            prev = current;
            current = (*current).next;
        }
        (*block).next = current;
        if prev.is_null() {
            *core::ptr::addr_of_mut!(FREE_HEAD) = block;
        } else {
            (*prev).next = block;
        }
        if !current.is_null() && block.cast::<u8>().add((*block).size) == current.cast::<u8>() {
            (*block).size += (*current).size;
            (*block).next = (*current).next;
        }
        if !prev.is_null() && prev.cast::<u8>().add((*prev).size) == block.cast::<u8>() {
            (*prev).size += (*block).size;
            (*prev).next = (*block).next;
        }
    });
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
