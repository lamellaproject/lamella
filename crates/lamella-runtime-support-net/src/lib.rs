//! Networking + TLS runtime support for the AOT tier: `#[no_mangle]` C-ABI entry points
//! over the SAME `NetBackend`/`TlsBackend` traits the interpreter binds -- one native
//! stack (smoltcp / mbedTLS), two thin consumers, so "runs on interpreter-P => runs on
//! device-P" holds for sockets and TLS by construction.

#![no_std]
#![allow(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;

use lamella_cil_runtime::net::{Interest, NetBackend, NetResult};
#[cfg(feature = "tls")]
use lamella_cil_runtime::tls::TlsBackend;

/// The managed poll loops' sentinels (Socket.cs): retry, and hard failure.
const WOULD_BLOCK: i32 = -1;
const SOCK_ERROR: i32 = -2;


/// The pinned RAM word holding the caller-provided region base (0 before init). On the device
/// it is a fixed low-RAM address adjacent to lamella_gc_alloc's HEAP_PTR (0x2000_0100); the AOT
/// host reserves it (confirm against the board RAM map). On the host (tests) a real `.bss` word
/// stands in, so the seam is exercised without the device address.
#[cfg(target_os = "none")]
#[inline]
fn rt_base_slot() -> *mut usize {
    0x2000_0104 as *mut usize
}
#[cfg(not(target_os = "none"))]
#[inline]
fn rt_base_slot() -> *mut usize {
    static mut HOST_SLOT: usize = 0;
    core::ptr::addr_of_mut!(HOST_SLOT)
}

const CURSOR_OFF: usize = 0;
const END_OFF: usize = 8;
const TICKS_OFF: usize = 16;
const STATE_OFF: usize = 24;

/// The mutable net/TLS backend cells, placed in the caller region at `base + STATE_OFF`.
#[repr(C)]
struct NetRtState {
    net: Option<Box<dyn NetBackend>>,
    #[cfg(feature = "tls")]
    tls: Option<Box<dyn TlsBackend>>,
}

/// The caller region base, or 0 before [`lamella_net_rt_init_ram`].
#[inline]
fn rt_base() -> usize {
    unsafe { *rt_base_slot() }
}

/// The net/TLS backend cells in the caller region, or `None` before init. The reference covers
/// ONLY the [`NetRtState`] (not the allocator's cursor/end/ticks words), so allocating or
/// ticking the clock while it is held stays disjoint.
fn rt_state() -> Option<&'static mut NetRtState> {
    let base = rt_base();
    if base == 0 {
        return None;
    }
    Some(unsafe { &mut *((base + STATE_OFF) as *mut NetRtState) })
}

/// Bump-allocate `layout` from the caller region -- RAW cursor/end words only, so it is safe to
/// call (e.g. from a `net.connect()` that allocates) while a [`NetRtState`] cell is borrowed.
/// Returns null before init or on exhaustion (the global allocator's OOM signal).
pub fn rt_alloc(layout: core::alloc::Layout) -> *mut u8 {
    let base = rt_base();
    if base == 0 {
        return core::ptr::null_mut();
    }
    unsafe {
        let cursor = (base + CURSOR_OFF) as *mut usize;
        let end = *((base + END_OFF) as *const usize);
        let align = layout.align().max(core::mem::size_of::<usize>());
        let start = (*cursor + align - 1) & !(align - 1);
        let stop = start + layout.size();
        if stop > end {
            return core::ptr::null_mut();
        }
        *cursor = stop;
        start as *mut u8
    }
}

/// Install the net/TLS runtime state over a caller-provided RAM region `[base, base+len)`. The
/// AOT image's boot calls this ONCE before any net/TLS entry -- the archive has no writable-data
/// segment of its own, so the host hands it a RAM block here. `base` must be 8-aligned and the
/// region unused by anything else for the program's lifetime.
///
/// # Safety
/// `base` must point to `len` writable bytes that outlive every net/TLS call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_net_rt_init_ram(base: *mut u8, len: usize) {
    let base = base as usize;
    unsafe {
        core::ptr::write(
            (base + STATE_OFF) as *mut NetRtState,
            NetRtState {
                net: None,
                #[cfg(feature = "tls")]
                tls: None,
            },
        );
        let heap_start = (base + STATE_OFF + core::mem::size_of::<NetRtState>() + 7) & !7;
        *((base + CURSOR_OFF) as *mut usize) = heap_start;
        *((base + END_OFF) as *mut usize) = base + len;
        *((base + TICKS_OFF) as *mut u64) = 0;
        *rt_base_slot() = base;
    }
}

/// Installs the networking backend the C-ABI entries drive. The board firmware calls this once
/// at boot (after [`lamella_net_rt_init_ram`]) with its `SmoltcpNet<BoardDevice>`; a second call
/// replaces the first (dropping any open sockets with the old backend).
pub fn install_net(backend: Box<dyn NetBackend>) {
    if let Some(state) = rt_state() {
        state.net = Some(backend);
    }
}

/// Installs a smoltcp LOOPBACK stack at 127.0.0.1/8 -- no MAC, no board: the QEMU / e2e
/// vehicle. `now_ms` is the image's monotonic millisecond clock (QEMU images derive one
/// from a counter loop; boards use their timer).
pub fn install_loopback(now_ms: fn() -> u64) {
    let device = smoltcp::phy::Loopback::new(smoltcp::phy::Medium::Ethernet);
    let mut config = lamella_net_smoltcp::NetConfig::new(
        [0x02, 0, 0, 0, 0, 0x01],
        lamella_net_smoltcp::IpSetup::Static {
            addr: [127, 0, 0, 1],
            prefix_len: 8,
            gateway: None,
            dns: alloc::vec::Vec::new(),
        },
    );
    config.would_block_grace_ms = 50;
    install_net(Box::new(lamella_net_smoltcp::SmoltcpNet::new(
        device, config, now_ms, 0x6c61_6d65_6c6c_6100,
    )));
}

/// A monotonic stand-in clock for images with no timer wired yet (the QEMU loopback
/// path): every read advances one "millisecond", so smoltcp timestamps stay monotonic and
/// the in-op grace window always makes progress in a bounded number of calls.
fn counting_now_ms() -> u64 {
    let base = rt_base();
    if base == 0 {
        return 0;
    }
    unsafe {
        let ticks = (base + TICKS_OFF) as *mut u64;
        *ticks += 1;
        *ticks
    }
}

/// The C-ABI twin of [`install_loopback`] for a reset stub that cannot call Rust
/// generics: wires the loopback stack over the internal counting clock. An AOT image's
/// boot sequence calls this once before the program entry.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_net_support_init_loopback() {
    install_loopback(counting_now_ms);
}

/// Runs `operation` on the installed backend, or reports the error sentinel.
fn with_net<T>(error: T, operation: impl FnOnce(&mut dyn NetBackend) -> T) -> T {
    match rt_state().and_then(|state| state.net.as_deref_mut()) {
        Some(backend) => operation(backend),
        None => error,
    }
}

/// Folds a `NetResult<usize>` into the managed sentinel space.
fn fold(result: NetResult<usize>) -> i32 {
    match result {
        NetResult::Ready(n) => n as i32,
        NetResult::WouldBlock => WOULD_BLOCK,
        NetResult::Error => SOCK_ERROR,
    }
}

/// A borrowed byte slice from a raw `(ptr, len)` pair (an empty slice for a null pointer,
/// so a zero-length managed array crosses safely).
///
/// # Safety
/// `ptr` must reference `len` readable bytes when non-null (the AOT passes `array + 4`
/// with the array's own length word, which upholds this).
unsafe fn in_slice<'a>(ptr: *const u8, len: u32) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(ptr, len as usize) }
    }
}

/// The mutable twin of [`in_slice`].
///
/// # Safety
/// `ptr` must reference `len` writable bytes when non-null.
unsafe fn out_slice<'a>(ptr: *mut u8, len: u32) -> &'a mut [u8] {
    if ptr.is_null() || len == 0 {
        &mut []
    } else {
        unsafe { core::slice::from_raw_parts_mut(ptr, len as usize) }
    }
}

/// `Socket.ConnectStart(addr, port)`: opens a TCP socket and begins connecting; the
/// handle, or an error sentinel. `addr` = 4 (IPv4) or 16 (IPv6) network-order bytes.
///
/// # Safety
/// `addr` must reference `addr_len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_net_connect_start(addr: *const u8, addr_len: u32, port: u32) -> i32 {
    let address = unsafe { in_slice(addr, addr_len) };
    with_net(SOCK_ERROR, |net| match net.tcp_connect(address, port as u16) {
        NetResult::Ready(handle) => handle as i32,
        NetResult::WouldBlock => WOULD_BLOCK,
        NetResult::Error => SOCK_ERROR,
    })
}

/// `Socket.ConnectPoll(handle)`: 0 connected, -1 still connecting, -2 failed.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_net_connect_poll(handle: i32) -> i32 {
    with_net(SOCK_ERROR, |net| match net.connect_check(handle as u32) {
        NetResult::Ready(()) => 0,
        NetResult::WouldBlock => {
            net.register(handle as u32, Interest::Write);
            WOULD_BLOCK
        }
        NetResult::Error => SOCK_ERROR,
    })
}

/// `Socket.ListenStart(addr, port, backlog)`: a bound + listening handle, or an error.
///
/// # Safety
/// `addr` must reference `addr_len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_net_listen_start(
    addr: *const u8,
    addr_len: u32,
    port: u32,
    backlog: i32,
) -> i32 {
    let address = unsafe { in_slice(addr, addr_len) };
    with_net(SOCK_ERROR, |net| {
        match net.tcp_listen(address, port as u16, backlog) {
            NetResult::Ready(handle) => handle as i32,
            NetResult::WouldBlock => WOULD_BLOCK,
            NetResult::Error => SOCK_ERROR,
        }
    })
}

/// `Socket.AcceptPoll(handle)`: a connected socket handle, -1 none pending, -2 error.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_net_accept_poll(handle: i32) -> i32 {
    with_net(SOCK_ERROR, |net| match net.accept(handle as u32) {
        NetResult::Ready(socket) => socket as i32,
        NetResult::WouldBlock => {
            net.register(handle as u32, Interest::Read);
            WOULD_BLOCK
        }
        NetResult::Error => SOCK_ERROR,
    })
}

/// `Socket.SendPoll(handle, buffer, offset, count)` with the slice pre-offset by the
/// caller: bytes accepted, -1 would-block, -2 error.
///
/// # Safety
/// `buf` must reference `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_net_send_poll(handle: i32, buf: *const u8, len: u32) -> i32 {
    let bytes = unsafe { in_slice(buf, len) };
    with_net(SOCK_ERROR, |net| {
        let result = net.send(handle as u32, bytes);
        if matches!(result, NetResult::WouldBlock) {
            net.register(handle as u32, Interest::Write);
        }
        fold(result)
    })
}

/// `Socket.ReceivePoll(handle, buffer, offset, count)` with the slice pre-offset by the
/// caller: bytes read (0 = clean close), -1 would-block, -2 error.
///
/// # Safety
/// `buf` must reference `len` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_net_recv_poll(handle: i32, buf: *mut u8, len: u32) -> i32 {
    let bytes = unsafe { out_slice(buf, len) };
    with_net(SOCK_ERROR, |net| {
        let result = net.recv(handle as u32, bytes);
        if matches!(result, NetResult::WouldBlock) {
            net.register(handle as u32, Interest::Read);
        }
        fold(result)
    })
}

/// `Socket.LocalPort(handle)`: the bound port, or -1.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_net_local_port(handle: i32) -> i32 {
    with_net(-1, |net| match net.local_port(handle as u32) {
        Some(port) => i32::from(port),
        None => -1,
    })
}

/// `Socket.CloseSocket(handle)`.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_net_close(handle: i32) {
    with_net((), |net| net.close(handle as u32));
}

#[cfg(feature = "tls")]
mod tls {
    use alloc::boxed::Box;
    use lamella_cil_runtime::tls::{TlsBackend, TlsStack, VerifyMode};

    use super::{in_slice, out_slice};

    /// Installs the TLS engine. The board calls this at boot (after [`super::lamella_net_rt_init_ram`]
    /// and after registering its entropy source with [`lamella_tls_mbedtls::set_entropy_source`]).
    /// The engine cell lives in the caller-provided region (see the module's writable-data note),
    /// alongside the net backend -- the same single-threaded contract.
    pub fn install_tls(backend: Box<dyn TlsBackend>) {
        if let Some(state) = super::rt_state() {
            state.tls = Some(backend);
        }
    }

    /// Installs the vendored mbedTLS engine (the device default).
    pub fn install_mbedtls() {
        install_tls(Box::new(lamella_tls_mbedtls::MbedTlsDevice::new()));
    }

    fn with_tls<T>(error: T, operation: impl FnOnce(&mut dyn TlsBackend) -> T) -> T {
        match super::rt_state().and_then(|state| state.tls.as_deref_mut()) {
            Some(backend) => operation(backend),
            None => error,
        }
    }

    /// `TlsNative.DefaultStack()`: the single compiled-in stack (mbedTLS = 1).
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_tls_default_stack() -> i32 {
        with_tls(1, |tls| tls.default_stack())
    }

    /// `TlsNative.ClientConfig(stack, verifyMode, rootsPem)` -- the PEM is unused on the
    /// device rungs (SystemRoots is refused, AcceptAny/none carry no roots), so the ABI
    /// takes only the two selectors. A config handle, or a negative failure.
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_tls_client_config(stack: i32, verify: i32) -> i32 {
        with_tls(-1, |tls| {
            match tls.client_config(TlsStack::from_i32(stack), VerifyMode::from_i32(verify), None)
            {
                Some(config) => config as i32,
                None => -1,
            }
        })
    }

    /// `TlsNative.ClientNew(config, targetHost)`: a session handle, or a negative failure.
    /// `host` is UTF-8 (the AOT lowers the managed string's units; ASCII hostnames are
    /// identical in both encodings, and SNI is ASCII by construction).
    ///
    /// # Safety
    /// `host` must reference `host_len` readable bytes.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn lamella_tls_client_new(config: i32, host: *const u8, host_len: u32) -> i32 {
        let name = unsafe { in_slice(host, host_len) };
        let name = core::str::from_utf8(name).unwrap_or("");
        with_tls(-1, |tls| match tls.client_new(config as u32, name) {
            Some(session) => session as i32,
            None => -1,
        })
    }

    /// `TlsNative.Process(tls)`: the session state (0 handshaking / 1 established /
    /// 2 closed / 3 error).
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_tls_process(tls: i32) -> i32 {
        with_tls(3, |engine| engine.process(tls as u32).as_i32())
    }

    /// `TlsNative.WantsWrite(tls)`: 1 when outgoing ciphertext is queued.
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_tls_wants_write(tls: i32) -> i32 {
        with_tls(0, |engine| i32::from(engine.wants_write(tls as u32)))
    }

    /// `TlsNative.WriteTls(tls, buffer, offset, count)` pre-offset: ciphertext drained
    /// into the buffer.
    ///
    /// # Safety
    /// `out` must reference `len` writable bytes.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn lamella_tls_write_tls(tls: i32, out: *mut u8, len: u32) -> i32 {
        let buffer = unsafe { out_slice(out, len) };
        with_tls(0, |engine| engine.write_tls(tls as u32, buffer) as i32)
    }

    /// `TlsNative.ReadTls(tls, buffer, offset, count)` pre-offset: ciphertext consumed
    /// from the buffer.
    ///
    /// # Safety
    /// `input` must reference `len` readable bytes.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn lamella_tls_read_tls(tls: i32, input: *const u8, len: u32) -> i32 {
        let buffer = unsafe { in_slice(input, len) };
        with_tls(0, |engine| engine.read_tls(tls as u32, buffer) as i32)
    }

    /// `TlsNative.ReadPlain(tls, buffer, offset, count)` pre-offset: plaintext read, 0 =
    /// none available yet, -2 = the peer closed (the managed `PlainClosed`).
    ///
    /// # Safety
    /// `out` must reference `len` writable bytes.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn lamella_tls_read_plain(tls: i32, out: *mut u8, len: u32) -> i32 {
        let buffer = unsafe { out_slice(out, len) };
        with_tls(-2, |engine| match engine.read_plain(tls as u32, buffer) {
            Some(read) => read as i32,
            None => -2,
        })
    }

    /// `TlsNative.WritePlain(tls, buffer, offset, count)` pre-offset: plaintext accepted
    /// for encryption.
    ///
    /// # Safety
    /// `input` must reference `len` readable bytes.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn lamella_tls_write_plain(tls: i32, input: *const u8, len: u32) -> i32 {
        let buffer = unsafe { in_slice(input, len) };
        with_tls(0, |engine| engine.write_plain(tls as u32, buffer) as i32)
    }

    /// `TlsNative.PeerCert(tls, buffer)`: the peer certificate's full DER length (written
    /// when it fits), 0 when none.
    ///
    /// # Safety
    /// `out` must reference `len` writable bytes.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn lamella_tls_peer_cert(tls: i32, out: *mut u8, len: u32) -> i32 {
        let buffer = unsafe { out_slice(out, len) };
        with_tls(0, |engine| engine.peer_cert(tls as u32, buffer) as i32)
    }

    /// `TlsNative.CloseTls(tls)`.
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_tls_close(tls: i32) {
        with_tls((), |engine| engine.close(tls as u32));
    }
}

#[cfg(feature = "tls")]
pub use tls::{install_mbedtls, install_tls};

#[cfg(test)]
mod tests {
    use super::*;

    /// An 8-aligned stand-in for the RAM region the AOT host hands `lamella_net_rt_init_ram`.
    #[repr(align(8))]
    struct Region([u8; 4096]);

    #[test]
    fn relocation_bumps_caller_ram_and_places_the_state_cell() {
        let mut region = Region([0u8; 4096]);
        let base = region.0.as_mut_ptr();
        unsafe { lamella_net_rt_init_ram(base, 4096) };

        let state = rt_state().expect("the state cell is reachable after init");
        assert!(state.net.is_none());

        let layout = core::alloc::Layout::from_size_align(16, 8).unwrap();
        let a = rt_alloc(layout);
        let b = rt_alloc(layout);
        assert!(!a.is_null() && !b.is_null());
        assert!(b as usize >= a as usize + 16, "allocations bump upward");
        assert!(a as usize >= base as usize, "allocation is inside the region");
        assert!((b as usize) < base as usize + 4096, "allocation stays in bounds");

        let huge = core::alloc::Layout::from_size_align(8192, 8).unwrap();
        assert!(rt_alloc(huge).is_null(), "an over-budget request reports OOM as null");
    }
}
