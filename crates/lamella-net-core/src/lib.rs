#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The networking seam: non-blocking sockets plus a readiness poll, behind a trait the embedder
//! supplies (host = `std::net` + `mio`; a device = lwIP / smoltcp / a Wi-Fi module / an AT modem; a
//! browser = WebSocket/fetch).

extern crate alloc;

use alloc::vec::Vec;

/// Per-socket operation timeouts: shared behavior a language runtime composes, so two languages on
/// one board answer "what does a receive timeout do here" the same way.
///
/// Deliberately not part of [`NetBackend`]. A timeout is implemented by waking a parked thread
/// earlier, which is the scheduler's business -- no backend is asked and none would have anything
/// to implement.
pub mod timeout;

/// One stack, several holders: a device brings its network up once at boot, and both a managed
/// program and a debug session reaching the board over that network need it.
pub mod shared;

/// A socket the backend hands out: an index into the backend's own table, opaque to the caller
/// (which just passes the handle back to identify the socket). Kept distinct from a raw fd so the
/// seam stays host-agnostic.
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

/// The networking seam. `Debug` is a supertrait so that a runtime holding an
/// `Option<Box<dyn NetBackend>>` still derives `Debug`.
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


    /// Whether any interface has a USABLE connection -- link up AND an IPv4 address assigned -- which
    /// is the honest meaning of "can I open a socket now". Backs `NetworkInterface.GetIsNetworkAvailable()`.
    fn network_available(&mut self) -> bool {
        false
    }

    /// How many interfaces this backend exposes (most devices: exactly one). Backs
    /// `NetworkInterface.GetAllNetworkInterfaces()` (the managed side builds one wrapper per index).
    fn interface_count(&mut self) -> u32 {
        0
    }

    /// A LIVE snapshot of interface `index` (`0..interface_count`), or `None` if out of range. Read
    /// fresh on each managed property get, so `OperationalStatus` / `IPv4Address` reflect the CURRENT
    /// link -- a cable pulled between two reads shows `Down`, a DHCP lease that just bound shows the
    /// new address.
    fn interface_info(&mut self, index: u32) -> Option<InterfaceInfo> {
        let _ = index;
        None
    }
}

/// The operational state of an interface. The discriminants are the .NET
/// `System.Net.NetworkInformation.OperationalStatus` values, so a status crosses the intrinsic seam
/// as exactly the integer the managed enum carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OperStatus {
    /// Up and able to pass packets.
    Up = 1,
    /// Down -- no link (cable out, radio disassociated).
    Down = 2,
    /// In a test mode; cannot pass packets.
    Testing = 3,
    /// State cannot be determined.
    Unknown = 4,
    /// Link present but pending some external action (e.g. awaiting DHCP).
    Dormant = 5,
    /// The hardware is missing.
    NotPresent = 6,
    /// A lower-layer interface this one stacks on is down.
    LowerLayerDown = 7,
}

/// The interface medium. The discriminants are the IANA ifType values .NET's `NetworkInterfaceType`
/// uses (Ethernet 6, Loopback 24, Wireless80211 71), so the value crosses the seam unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IfaceKind {
    /// The medium is unknown.
    Unknown = 1,
    /// Wired Ethernet (IANA `ethernetCsmacd`).
    Ethernet = 6,
    /// A PPP serial/cellular link.
    Ppp = 23,
    /// A software loopback interface.
    Loopback = 24,
    /// IEEE 802.11 wireless (Wi-Fi).
    Wireless80211 = 71,
}

/// A live snapshot of one interface -- the data behind the managed `NetworkInterface` poll surface.
/// IPv4 fields are big-endian octets; `[0, 0, 0, 0]` means "unassigned" (e.g. DHCP not yet bound).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InterfaceInfo {
    /// Up / Down / Dormant / ... -- the current link state.
    pub oper_status: OperStatus,
    /// Ethernet / Wireless80211 / Loopback / ... -- the medium.
    pub kind: IfaceKind,
    /// The interface's IPv4 address (big-endian octets; `[0,0,0,0]` = none).
    pub ipv4: [u8; 4],
    /// The IPv4 subnet mask (big-endian octets).
    pub subnet: [u8; 4],
    /// The IPv4 default gateway (big-endian octets; `[0,0,0,0]` = none).
    pub gateway: [u8; 4],
    /// Whether the address was obtained by DHCP (vs a static configuration).
    pub dhcp_enabled: bool,
}

/// Watches a backend's IPv4 address and reports each CHANGE exactly once.
///
/// # Why a bring-up window is not enough on its own
///
/// A bring-up that waits a bounded time for DHCP and then narrates the outcome publishes a
/// VERDICT, and the stack can go on to contradict it -- the lease arrives after the wait gave up.
/// Lengthening the wait does not settle it either, because a lease can arrive with no relation to
/// boot at all: a renewal after an expiry is the ordinary case. What a firmware can do reliably is
/// say so at the moment it happens. This type is the single implementation of that detection, so
/// several narration sites cannot each get the transition subtly wrong.
///
/// # A loss is a change too
///
/// `[0, 0, 0, 0]` is this seam's spelling of "unassigned", so losing an address is a change to
/// `None` and is worth reporting: a board that has quietly stopped being reachable otherwise looks
/// exactly like a board nobody has connected to.
///
/// # What it reads
///
/// Interface 0, through [`NetBackend::interface_info`], whose default implementation returns
/// `None`. A backend that does not implement it reports no interface, so this watch stays silent
/// -- which is correct for a backend that has no interfaces, and indistinguishable from a backend
/// that simply has not overridden the method. Confirm that a backend answers `interface_info`
/// before reading this watch's silence as "nothing changed".
pub struct AddressWatch {
    /// The address last REPORTED, so a change is measured against what a reader has been told
    /// rather than against the previous poll.
    reported: Option<[u8; 4]>,
    /// Whether anything has been reported yet, which is what separates "no address, as before"
    /// from a first look that happens to find none.
    started: bool,
}

impl Default for AddressWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl AddressWatch {
    /// A watch that has reported nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self { reported: None, started: false }
    }

    /// The address now, if it DIFFERS from what was last reported; otherwise `None`.
    ///
    /// The first call reports whatever it finds, including `None` for a board with no address --
    /// a caller that only wants to hear about a gain can ignore a `Some(None)`.
    pub fn changed(&mut self, net: &mut dyn NetBackend) -> Option<Option<[u8; 4]>> {
        let current = match net.interface_info(0) {
            Some(info) if info.ipv4 != [0, 0, 0, 0] => Some(info.ipv4),
            _ => None,
        };
        if self.started && current == self.reported {
            return None;
        }
        self.started = true;
        self.reported = current;
        Some(current)
    }

    /// The address last reported, without polling the backend.
    #[must_use]
    pub fn reported(&self) -> Option<[u8; 4]> {
        self.reported
    }
}

#[cfg(test)]
mod address_watch_tests {
    use super::*;

    /// A backend that answers only [`NetBackend::interface_info`], because that is the whole of
    /// what [`AddressWatch`] reads. Every other operation panics rather than returning a plausible
    /// value: a watch that started using one would be doing something this type does not claim.
    #[derive(Debug)]
    struct OneIface {
        ipv4: Option<[u8; 4]>,
        /// Whether the backend reports an interface at all -- the case a backend that has not
        /// overridden `interface_info` presents, which must read as silence and not as a loss.
        present: bool,
    }

    impl OneIface {
        fn at(ipv4: Option<[u8; 4]>) -> Self {
            Self { ipv4, present: true }
        }
    }

    impl NetBackend for OneIface {
        fn resolve(&mut self, _host: &str) -> Vec<Vec<u8>> {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn tcp_connect(&mut self, _addr: &[u8], _port: u16) -> NetResult<SocketHandle> {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn connect_check(&mut self, _socket: SocketHandle) -> NetResult<()> {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn tcp_listen(&mut self, _addr: &[u8], _port: u16, _backlog: i32) -> NetResult<SocketHandle> {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn accept(&mut self, _listener: SocketHandle) -> NetResult<SocketHandle> {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn recv(&mut self, _socket: SocketHandle, _buf: &mut [u8]) -> NetResult<usize> {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn send(&mut self, _socket: SocketHandle, _buf: &[u8]) -> NetResult<usize> {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn udp_bind(&mut self, _addr: &[u8], _port: u16) -> NetResult<SocketHandle> {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn udp_send_to(
            &mut self,
            _socket: SocketHandle,
            _buf: &[u8],
            _addr: &[u8],
            _port: u16,
        ) -> NetResult<usize> {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn udp_recv_from(
            &mut self,
            _socket: SocketHandle,
            _buf: &mut [u8],
            _sender_addr: &mut [u8],
        ) -> NetResult<(usize, usize, u16)> {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn local_port(&mut self, _socket: SocketHandle) -> Option<u16> {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn close(&mut self, _socket: SocketHandle) {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn register(&mut self, _socket: SocketHandle, _interest: Interest) {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn deregister(&mut self, _socket: SocketHandle) {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn poll(&mut self, _timeout_ms: Option<u64>) -> Vec<SocketHandle> {
            unimplemented!("AddressWatch reads interface_info only")
        }
        fn interface_info(&mut self, index: u32) -> Option<InterfaceInfo> {
            if !self.present || index != 0 {
                return None;
            }
            Some(InterfaceInfo {
                oper_status: OperStatus::Up,
                kind: IfaceKind::Ethernet,
                ipv4: self.ipv4.unwrap_or([0, 0, 0, 0]),
                subnet: [255, 255, 255, 0],
                gateway: [192, 168, 1, 1],
                dhcp_enabled: true,
            })
        }
    }

    /// THE CASE THIS TYPE EXISTS FOR: an address that arrives after the caller has already given
    /// up waiting for it is reported, and reported ONCE.
    #[test]
    fn a_late_address_is_reported_once() {
        let mut net = OneIface::at(None);
        let mut watch = AddressWatch::new();

        assert_eq!(watch.changed(&mut net), Some(None), "the first look reports what it finds");
        assert_eq!(watch.changed(&mut net), None, "still no address is not news");

        net.ipv4 = Some([192, 168, 1, 205]);
        assert_eq!(watch.changed(&mut net), Some(Some([192, 168, 1, 205])), "the lease lands");
        assert_eq!(watch.changed(&mut net), None, "and is not announced twice");
        assert_eq!(watch.reported(), Some([192, 168, 1, 205]));
    }

    /// A lease LOST is a change too. A board that has quietly stopped being reachable otherwise
    /// reads exactly like a board nobody has dialed.
    #[test]
    fn losing_the_address_is_a_change() {
        let mut net = OneIface::at(Some([10, 0, 0, 7]));
        let mut watch = AddressWatch::new();
        assert_eq!(watch.changed(&mut net), Some(Some([10, 0, 0, 7])));

        net.ipv4 = None;
        assert_eq!(watch.changed(&mut net), Some(None), "the address went away");
        assert_eq!(watch.changed(&mut net), None);
        assert_eq!(watch.reported(), None);
    }

    /// A RENEWAL ONTO A DIFFERENT ADDRESS is a change, not a repeat -- the case a watch that only
    /// asked "do we have one yet" would miss, leaving every host pointed at the old address.
    #[test]
    fn a_different_address_is_a_change_without_passing_through_none() {
        let mut net = OneIface::at(Some([192, 168, 1, 205]));
        let mut watch = AddressWatch::new();
        assert_eq!(watch.changed(&mut net), Some(Some([192, 168, 1, 205])));

        net.ipv4 = Some([192, 168, 1, 206]);
        assert_eq!(watch.changed(&mut net), Some(Some([192, 168, 1, 206])));
        assert_eq!(watch.changed(&mut net), None);
    }

    /// `[0,0,0,0]` is the seam's spelling of "unassigned", so it must read as NO address rather
    /// than as an address that happens to be zero -- otherwise a board with no lease reports
    /// itself reachable at 0.0.0.0.
    #[test]
    fn the_unassigned_address_is_not_an_address() {
        let mut net = OneIface::at(Some([0, 0, 0, 0]));
        let mut watch = AddressWatch::new();
        assert_eq!(watch.changed(&mut net), Some(None));
        assert_eq!(watch.reported(), None);
    }

    /// A backend that reports NO interface -- which is what the trait's own default does -- must
    /// look like silence, not like an address that was lost. This is the state every backend that
    /// has not overridden `interface_info` is in, and it must not manufacture an event.
    #[test]
    fn a_backend_with_no_interface_reports_no_address_and_then_nothing() {
        let mut net = OneIface { ipv4: None, present: false };
        let mut watch = AddressWatch::new();
        assert_eq!(watch.changed(&mut net), Some(None), "the first look still reports");
        assert_eq!(watch.changed(&mut net), None, "and then it is quiet forever");
    }
}
