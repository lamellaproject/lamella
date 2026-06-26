//! The host networking seam: non-blocking sockets plus a readiness poll, behind a trait the embedder
//! supplies (host = `std::net` + `mio`; a device = lwIP / an AT modem; a browser = WebSocket/fetch).

use alloc::vec::Vec;

/// A socket the backend hands out: an index into the backend's own table, opaque to the interpreter
/// (it just passes the handle back to identify the socket). Kept distinct from a raw fd so the seam
/// stays host-agnostic.
pub type SocketHandle = u32;

/// What a watched socket is waiting to become.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Interest {
    /// Readable -- a pending `recv` (data has arrived) or `accept` (a connection is pending).
    Read,
    /// Writable -- a pending `connect` has completed, or a full send buffer has drained.
    Write,
}

/// The outcome of a non-blocking socket operation.
pub enum NetResult<T> {
    /// Completed with this value.
    Ready(T),
    /// Cannot complete yet; the caller parks until the socket is ready for the matching [`Interest`].
    WouldBlock,
    /// Failed (the address is unreachable, the connection was reset, the socket is broken, ...).
    Error,
}

/// The networking seam. `Debug` is a supertrait so the [`crate::interp::Vm`] -- which holds an
/// `Option<Box<dyn NetBackend>>` -- still derives `Debug`.
pub trait NetBackend: core::fmt::Debug {
    /// Resolves a host name to its IP addresses -- each entry is the address bytes in network order
    /// (4 = IPv4, 16 = IPv6), in the host resolver's order. An empty vec means resolution failed. The
    /// managed `System.Net.Dns` builds an `IPAddress[]` from these (so both families + multiple
    /// addresses surface). (Async DNS is later.)
    fn resolve(&mut self, host: &str) -> alloc::vec::Vec<alloc::vec::Vec<u8>>;

    /// Opens a non-blocking TCP socket and begins connecting to `addr:port`. `addr` is the address
    /// bytes in network order (the first byte is the high-order octet) -- 4 for IPv4, 16 for IPv6.
    /// Returns the socket handle immediately; the connection may still be in progress -- the caller
    /// parks for [`Interest::Write`] until it completes (see [`NetBackend::connect_check`]).
    fn tcp_connect(&mut self, addr: &[u8], port: u16) -> NetResult<SocketHandle>;

    /// Whether a connecting socket has finished connecting: `Ready(())` connected, `WouldBlock` still
    /// connecting, `Error` the connect failed.
    fn connect_check(&mut self, socket: SocketHandle) -> NetResult<()>;

    /// Opens a non-blocking TCP listener bound to `addr:port` (4- or 16-byte `addr` in network order;
    /// port 0 = an ephemeral port, read back with [`NetBackend::local_port`]).
    fn tcp_listen(&mut self, addr: &[u8], port: u16, backlog: i32) -> NetResult<SocketHandle>;

    /// Accepts one pending connection on a listener, returning a new connected socket handle.
    fn accept(&mut self, listener: SocketHandle) -> NetResult<SocketHandle>;

    /// Non-blocking receive into `buf`; `Ready(n)` read `n` bytes (`0` = the peer closed cleanly).
    fn recv(&mut self, socket: SocketHandle, buf: &mut [u8]) -> NetResult<usize>;

    /// Non-blocking send from `buf`; `Ready(n)` wrote `n` bytes (possibly fewer than `buf.len()`).
    fn send(&mut self, socket: SocketHandle, buf: &[u8]) -> NetResult<usize>;

    /// Opens a non-blocking UDP socket bound to `addr:port` (4- or 16-byte `addr`; port 0 = ephemeral).
    fn udp_bind(&mut self, addr: &[u8], port: u16) -> NetResult<SocketHandle>;

    /// Sends a datagram from `buf` to `addr:port` (4- or 16-byte `addr`); `Ready(n)` wrote `n` bytes.
    fn udp_send_to(&mut self, socket: SocketHandle, buf: &[u8], addr: &[u8], port: u16) -> NetResult<usize>;

    /// Receives one datagram into `buf`, writing the sender's address (network order) into the front of
    /// `sender_addr`. `Ready((n, addr_len, port))`: `n` bytes read, the sender is `sender_addr[..addr_len]`
    /// (4 or 16) at `port`.
    fn udp_recv_from(
        &mut self,
        socket: SocketHandle,
        buf: &mut [u8],
        sender_addr: &mut [u8],
    ) -> NetResult<(usize, usize, u16)>;

    /// The local port a socket/listener is bound to, or `None`.
    fn local_port(&mut self, socket: SocketHandle) -> Option<u16>;

    /// Closes a socket or listener and releases its handle.
    fn close(&mut self, socket: SocketHandle);

    /// Registers (or updates) the interest a parked thread is waiting on, so the next [`poll`] watches
    /// `socket` for `interest`. Called by the scheduler when a socket op parks a thread. Re-registers a
    /// socket that was [`deregister`](NetBackend::deregister)ed after a prior wake.
    ///
    /// [`poll`]: NetBackend::poll
    fn register(&mut self, socket: SocketHandle, interest: Interest);

    /// Drops `socket` from the poll-set once the thread parked on it has been woken (the scheduler
    /// calls this in its reactor wake step). Keeps the poll-set to only sockets with a currently-parked
    /// waiter, so a stale registration never produces a spurious wake; a later [`register`] re-arms it.
    /// A no-op if the socket is not currently in the poll-set.
    ///
    /// [`register`]: NetBackend::register
    fn deregister(&mut self, socket: SocketHandle);

    /// Blocks until at least one registered socket is ready for its interest, or `timeout_ms` elapses
    /// (`None` = block indefinitely). Returns the handles now ready. The scheduler's single OS-thread
    /// block point, called only when every green thread is parked.
    fn poll(&mut self, timeout_ms: Option<u64>) -> Vec<SocketHandle>;
}
