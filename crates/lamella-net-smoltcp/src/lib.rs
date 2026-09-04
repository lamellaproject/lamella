//! The DEVICE networking backend for the interpreter's [`NetBackend`] seam: `smoltcp` (a
//! pure-Rust, `no_std` TCP/IP stack) over any [`smoltcp::phy::Device`] -- a board's Ethernet
//! MAC driver on hardware, `smoltcp`'s loopback device in tests. The device twin of the HOST
//! backend: the same non-blocking socket vocabulary, with `smoltcp`'s poll-driven interface as
//! the readiness reactor instead of the OS poller.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub use smoltcp;

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use lamella_net_core::{
    IfaceKind, Interest, InterfaceInfo, NetBackend, NetResult, OperStatus, SocketHandle,
};
use smoltcp::iface::{Config, Interface, SocketHandle as SmolHandle, SocketSet};
use smoltcp::phy::Device;
use smoltcp::socket::{dhcpv4, dns, tcp, udp};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{
    DnsQueryType, EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint,
    IpListenEndpoint, Ipv4Address,
};

/// The first local port handed out for connects and port-0 binds (the IANA dynamic range).
const EPHEMERAL_FIRST: u16 = 49152;

/// How long [`NetBackend::resolve`] drives the DNS query before giving up, in milliseconds.
const DNS_TIMEOUT_MS: u64 = 5_000;

/// Extra pool slots a listener may hold beyond its listening target: connections that
/// completed but have not been `accept`ed yet. At the cap, top-up pauses (a further
/// connection attempt is refused with RST until an accept drains the pool).
const LISTENER_PENDING_CAP: usize = 8;

/// How the interface obtains its IPv4 address.
#[derive(Clone, Debug)]
pub enum IpSetup {
    /// DHCP: a `smoltcp` DHCPv4 socket is driven inside the poll passes; the discovered
    /// address, default route, and DNS servers are applied to the interface as they arrive.
    Dhcp,
    /// A fixed address (plus optional default gateway and DNS servers).
    Static {
        /// The interface IPv4 address, network order.
        addr: [u8; 4],
        /// The subnet prefix length (e.g. 24 for a /24).
        prefix_len: u8,
        /// The default gateway, if any.
        gateway: Option<[u8; 4]>,
        /// DNS servers for [`NetBackend::resolve`] (up to 3 are used).
        dns: Vec<[u8; 4]>,
    },
}

/// Construction-time configuration: identity, addressing, and the buffer/pool sizing knobs.
#[derive(Clone, Debug)]
pub struct NetConfig {
    /// The interface MAC address.
    pub mac: [u8; 6],
    /// The IPv4 addressing mode.
    pub ip: IpSetup,
    /// Per-TCP-socket receive buffer bytes.
    pub tcp_rx_buffer: usize,
    /// Per-TCP-socket transmit buffer bytes.
    pub tcp_tx_buffer: usize,
    /// Per-UDP-socket payload buffer bytes, each direction.
    pub udp_buffer: usize,
    /// Per-UDP-socket datagram slots, each direction.
    pub udp_slots: usize,
    /// Sockets a listener keeps in the LISTEN state (concurrent handshakes it can absorb).
    pub listen_backlog: usize,
    /// The cap applied to [`NetBackend::poll`] with no timeout. A device serve loop must
    /// regain control to answer its wire (a reclaim HELLO), so "block indefinitely" becomes
    /// "block this long, then report nothing ready" -- the scheduler simply polls again.
    pub max_block_ms: u64,
    /// How long a would-block operation (accept/recv/connect-check/send) keeps DRIVING the
    /// stack in-op before reporting [`NetResult::WouldBlock`], in milliseconds. Default 0 =
    /// report immediately (the reactor/scheduler shape a host embedder wants). A SCHEDULER-LESS
    /// embedder (a device boot-run stepping one session) sets a small grace instead: its
    /// managed code can only busy-retry the operation, and each retry costs interpreter work
    /// (and, on a bump arena, permanent allocations) -- the grace collapses that retry rate
    /// by orders of magnitude while the stack still makes progress inside the wait.
    pub would_block_grace_ms: u64,
}

impl NetConfig {
    /// A configuration with the given identity/addressing and default sizing: 4 KiB TCP
    /// buffers, 2 KiB / 8-slot UDP buffers, a 2-deep listen pool, a 100 ms poll cap.
    #[must_use]
    pub fn new(mac: [u8; 6], ip: IpSetup) -> Self {
        NetConfig {
            mac,
            ip,
            tcp_rx_buffer: 4096,
            tcp_tx_buffer: 4096,
            udp_buffer: 2048,
            udp_slots: 8,
            listen_backlog: 2,
            max_block_ms: 100,
            would_block_grace_ms: 0,
        }
    }
}

/// One seam socket, keyed by its [`SocketHandle`].
enum Entry {
    /// A connected (or connecting) TCP socket.
    Tcp(SmolHandle),
    /// A TCP listener: the pool of `smoltcp` sockets serving its endpoint (see the module
    /// docs for the pool adapter).
    Listener {
        /// The bound endpoint (specific address if one was given, else any-address).
        endpoint: IpListenEndpoint,
        /// Pool sockets: LISTEN, mid-handshake, or connected-awaiting-accept.
        pool: Vec<SmolHandle>,
    },
    /// A bound UDP socket.
    Udp(SmolHandle),
}

/// The `smoltcp`-backed [`NetBackend`]: an interface + socket set over the MAC driver `D`,
/// a handle table, and the poll-set of sockets a parked green thread is waiting on.
pub struct SmoltcpNet<D: Device> {
    device: D,
    iface: Interface,
    sockets: SocketSet<'static>,
    /// The monotonic millisecond clock (the embedder supplies it; tests tick a counter).
    now_ms: fn() -> u64,
    config: NetConfig,
    table: BTreeMap<SocketHandle, Entry>,
    /// Handles with a currently-parked waiter and the interest awaited -- only these are
    /// reported ready by [`NetBackend::poll`].
    armed: BTreeMap<SocketHandle, Interest>,
    /// Closed-by-us TCP sockets still draining their FIN handshake; reaped once CLOSED.
    closing: Vec<SmolHandle>,
    dhcp: Option<SmolHandle>,
    dns: SmolHandle,
    dns_servers: Vec<IpAddress>,
    /// The current IPv4 default gateway (the static config's, or the last DHCP lease's router), kept
    /// so the `NetworkInterface` poll surface can report it -- `smoltcp`'s routes are not readable
    /// back out. `None` = none configured.
    gateway: Option<[u8; 4]>,
    next_handle: SocketHandle,
    next_port: u16,
}

/// The `IpAddress` for 4 address bytes in network order; `None` for any other length
/// (IPv6 is outside this backend's IPv4 build).
fn ip_from_bytes(addr: &[u8]) -> Option<IpAddress> {
    let octets: [u8; 4] = addr.try_into().ok()?;
    Some(IpAddress::Ipv4(Ipv4Address::from(octets)))
}

/// The network-order address bytes of an `IpAddress`.
fn addr_bytes(ip: &IpAddress) -> Vec<u8> {
    match ip {
        IpAddress::Ipv4(v4) => v4.octets().to_vec(),
    }
}

/// The dotted IPv4 subnet mask for a CIDR prefix length (24 -> 255.255.255.0; 0 -> 0.0.0.0). Backs
/// the `NetworkInterface.IPv4SubnetMask` poll surface, since `smoltcp` stores the prefix, not a mask.
fn prefix_to_mask(prefix: u8) -> [u8; 4] {
    if prefix == 0 {
        return [0, 0, 0, 0];
    }
    (u32::MAX << (32 - u32::from(prefix.min(32)))).to_be_bytes()
}

impl<D: Device> SmoltcpNet<D> {
    /// Creates the backend over the MAC driver: builds the interface, applies the
    /// addressing mode, and readies the DNS/DHCP sockets. `now_ms` is the monotonic
    /// millisecond clock; `random_seed` feeds `smoltcp`'s sequence-number/port
    /// randomization (a device stirs in its unique id; tests pass a constant).
    pub fn new(mut device: D, config: NetConfig, now_ms: fn() -> u64, random_seed: u64) -> Self {
        let mut iface_config = Config::new(HardwareAddress::Ethernet(EthernetAddress(config.mac)));
        iface_config.random_seed = random_seed;
        let mut iface =
            Interface::new(iface_config, &mut device, Instant::from_millis(now_ms() as i64));
        let mut sockets = SocketSet::new(Vec::new());
        let mut dhcp = None;
        let mut dns_servers: Vec<IpAddress> = Vec::new();
        let mut gateway_addr: Option<[u8; 4]> = None;
        match &config.ip {
            IpSetup::Dhcp => {
                let mut socket = dhcpv4::Socket::new();
                let mut retry = dhcpv4::RetryConfig::default();
                retry.discover_timeout = Duration::from_secs(4);
                socket.set_retry_config(retry);
                dhcp = Some(sockets.add(socket));
            }
            IpSetup::Static { addr, prefix_len, gateway, dns } => {
                let ip = IpAddress::Ipv4(Ipv4Address::from(*addr));
                iface.update_ip_addrs(|addrs| {
                    let _ = addrs.push(IpCidr::new(ip, *prefix_len));
                });
                if let Some(gw) = gateway {
                    let _ = iface.routes_mut().add_default_ipv4_route(Ipv4Address::from(*gw));
                    gateway_addr = Some(*gw);
                }
                dns_servers =
                    dns.iter().map(|s| IpAddress::Ipv4(Ipv4Address::from(*s))).collect();
            }
        }
        dns_servers.truncate(3);
        let dns = sockets.add(dns::Socket::new(&dns_servers, Vec::new()));
        SmoltcpNet {
            device,
            iface,
            sockets,
            now_ms,
            config,
            table: BTreeMap::new(),
            armed: BTreeMap::new(),
            closing: Vec::new(),
            dhcp,
            dns,
            dns_servers,
            gateway: gateway_addr,
            next_handle: 1,
            next_port: EPHEMERAL_FIRST,
        }
    }

    /// The interface's current IPv4 address, once configured (static immediately; DHCP
    /// after a lease arrives). A firmware uses this to report the board's address.
    pub fn ipv4_addr(&self) -> Option<[u8; 4]> {
        self.iface.ipv4_addr().map(|a| a.octets())
    }

    fn now(&self) -> Instant {
        Instant::from_millis((self.now_ms)() as i64)
    }

    /// Reserves the next seam handle.
    fn fresh(&mut self) -> SocketHandle {
        let handle = self.next_handle;
        self.next_handle += 1;
        handle
    }

    /// The next local port for a connect or a port-0 bind.
    fn ephemeral_port(&mut self) -> u16 {
        let port = self.next_port;
        self.next_port = if self.next_port == u16::MAX { EPHEMERAL_FIRST } else { self.next_port + 1 };
        port
    }

    /// One full progress pass: move frames through the interface, apply any DHCP event,
    /// keep listener pools stocked, reap FIN-drained sockets. Every seam operation runs
    /// this so the stack advances even when the caller never parks (see the module docs).
    fn drive(&mut self) {
        let timestamp = self.now();
        let _ = self.iface.poll(timestamp, &mut self.device, &mut self.sockets);
        self.service_dhcp();
        self.top_up_listeners();
        self.reap_closed();
    }

    /// Applies a DHCP configure/deconfigure event to the interface, routes, and DNS list.
    fn service_dhcp(&mut self) {
        let Some(handle) = self.dhcp else { return };
        match self.sockets.get_mut::<dhcpv4::Socket>(handle).poll() {
            None => {}
            Some(dhcpv4::Event::Configured(lease)) => {
                self.iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    let _ = addrs.push(IpCidr::Ipv4(lease.address));
                });
                match lease.router {
                    Some(router) => {
                        let _ = self.iface.routes_mut().add_default_ipv4_route(router);
                    }
                    None => {
                        self.iface.routes_mut().remove_default_ipv4_route();
                    }
                }
                self.gateway = lease.router.map(|router| router.octets());
                self.dns_servers =
                    lease.dns_servers.iter().map(|s| IpAddress::Ipv4(*s)).collect();
                self.dns_servers.truncate(3);
                self.sockets.get_mut::<dns::Socket>(self.dns).update_servers(&self.dns_servers);
            }
            Some(dhcpv4::Event::Deconfigured) => {
                self.iface.update_ip_addrs(|addrs| addrs.clear());
                self.iface.routes_mut().remove_default_ipv4_route();
                self.gateway = None;
            }
        }
    }

    /// Keeps each listener pool holding `listen_backlog` sockets in LISTEN (up to the
    /// pending cap) and drops pool sockets that died before an accept (e.g. reset).
    fn top_up_listeners(&mut self) {
        let rx_bytes = self.config.tcp_rx_buffer;
        let tx_bytes = self.config.tcp_tx_buffer;
        let target = self.config.listen_backlog.max(1);
        for entry in self.table.values_mut() {
            let Entry::Listener { endpoint, pool } = entry else { continue };
            let mut index = 0;
            while index < pool.len() {
                if self.sockets.get::<tcp::Socket>(pool[index]).state() == tcp::State::Closed {
                    self.sockets.remove(pool[index]);
                    pool.swap_remove(index);
                } else {
                    index += 1;
                }
            }
            let listening = pool
                .iter()
                .filter(|&&h| self.sockets.get::<tcp::Socket>(h).state() == tcp::State::Listen)
                .count();
            let cap = target + LISTENER_PENDING_CAP;
            for _ in listening..target {
                if pool.len() >= cap {
                    break;
                }
                let rx = tcp::SocketBuffer::new(vec![0u8; rx_bytes]);
                let tx = tcp::SocketBuffer::new(vec![0u8; tx_bytes]);
                let mut socket = tcp::Socket::new(rx, tx);
                if socket.listen(*endpoint).is_ok() {
                    pool.push(self.sockets.add(socket));
                }
            }
        }
    }

    /// Removes closed-by-us TCP sockets whose FIN handshake has fully drained.
    fn reap_closed(&mut self) {
        let mut index = 0;
        while index < self.closing.len() {
            let smol = self.closing[index];
            if self.sockets.get::<tcp::Socket>(smol).state() == tcp::State::Closed {
                self.sockets.remove(smol);
                self.closing.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }

    /// Whether a pool socket has completed a handshake and is ready to be `accept`ed
    /// (connected -- or connected and the peer already closed its half).
    fn acceptable(state: tcp::State) -> bool {
        matches!(state, tcp::State::Established | tcp::State::CloseWait)
    }

    /// Whether `handle` is ready for `interest` -- the reactor's readiness predicate,
    /// matching what the corresponding seam call would do:
    /// a `Read`-parked TCP socket wakes for data or a closed receive half (EOF/reset), a
    /// `Write`-parked one for send room or a dead connection (a failed connect must wake
    /// its `connect_check`), a listener for an acceptable pool socket, UDP for a queued
    /// datagram / send room.
    fn is_ready(&self, entry: &Entry, interest: Interest) -> bool {
        match entry {
            Entry::Tcp(smol) => {
                let socket = self.sockets.get::<tcp::Socket>(*smol);
                match interest {
                    Interest::Read => socket.can_recv() || !socket.may_recv(),
                    Interest::Write => socket.can_send() || socket.state() == tcp::State::Closed,
                }
            }
            Entry::Listener { pool, .. } => {
                interest == Interest::Read
                    && pool
                        .iter()
                        .any(|&h| Self::acceptable(self.sockets.get::<tcp::Socket>(h).state()))
            }
            Entry::Udp(smol) => {
                let socket = self.sockets.get::<udp::Socket>(*smol);
                match interest {
                    Interest::Read => socket.can_recv(),
                    Interest::Write => socket.can_send(),
                }
            }
        }
    }

    /// The armed handles currently ready for their interest.
    fn ready_armed(&self) -> Vec<SocketHandle> {
        let mut ready = Vec::new();
        for (&handle, &interest) in &self.armed {
            if let Some(entry) = self.table.get(&handle) {
                if self.is_ready(entry, interest) {
                    ready.push(handle);
                }
            }
        }
        ready
    }

    /// Runs `operation` until it stops reporting `None` ("would block") or the configured
    /// grace passes, driving the stack between checks -- the scheduler-less embedder's
    /// in-op wait (see [`NetConfig::would_block_grace_ms`]). With no grace configured the
    /// single upfront check is all that runs.
    fn with_grace<T>(&mut self, mut operation: impl FnMut(&mut Self) -> Option<T>) -> Option<T> {
        if let Some(result) = operation(self) {
            return Some(result);
        }
        if self.config.would_block_grace_ms == 0 {
            return None;
        }
        let deadline = (self.now_ms)().saturating_add(self.config.would_block_grace_ms);
        while (self.now_ms)() < deadline {
            self.drive();
            if let Some(result) = operation(self) {
                return Some(result);
            }
        }
        None
    }

    /// A dotted-quad IPv4 literal (or "localhost"), resolved without a DNS query.
    fn literal(host: &str) -> Option<Vec<Vec<u8>>> {
        if host == "localhost" {
            return Some(vec![vec![127, 0, 0, 1]]);
        }
        host.parse::<core::net::Ipv4Addr>().ok().map(|ip| vec![ip.octets().to_vec()])
    }
}

impl<D: Device> core::fmt::Debug for SmoltcpNet<D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SmoltcpNet")
            .field("sockets", &self.table.len())
            .field("armed", &self.armed.len())
            .field("dhcp", &self.dhcp.is_some())
            .finish()
    }
}

impl<D: Device> NetBackend for SmoltcpNet<D> {
    fn resolve(&mut self, host: &str) -> Vec<Vec<u8>> {
        if let Some(literal) = Self::literal(host) {
            return literal;
        }
        if self.dns_servers.is_empty() {
            return Vec::new();
        }
        let query = match self
            .sockets
            .get_mut::<dns::Socket>(self.dns)
            .start_query(self.iface.context(), host, DnsQueryType::A)
        {
            Ok(query) => query,
            Err(_) => return Vec::new(),
        };
        let deadline = (self.now_ms)().saturating_add(DNS_TIMEOUT_MS);
        loop {
            self.drive();
            match self.sockets.get_mut::<dns::Socket>(self.dns).get_query_result(query) {
                Ok(addrs) => return addrs.iter().map(addr_bytes).collect(),
                Err(dns::GetQueryResultError::Pending) => {}
                Err(dns::GetQueryResultError::Failed) => return Vec::new(),
            }
            if (self.now_ms)() >= deadline {
                self.sockets.get_mut::<dns::Socket>(self.dns).cancel_query(query);
                return Vec::new();
            }
        }
    }

    fn tcp_connect(&mut self, addr: &[u8], port: u16) -> NetResult<SocketHandle> {
        let Some(remote) = ip_from_bytes(addr) else {
            return NetResult::Error;
        };
        let rx = tcp::SocketBuffer::new(vec![0u8; self.config.tcp_rx_buffer]);
        let tx = tcp::SocketBuffer::new(vec![0u8; self.config.tcp_tx_buffer]);
        let mut socket = tcp::Socket::new(rx, tx);
        let local_port = self.ephemeral_port();
        if socket
            .connect(self.iface.context(), IpEndpoint::new(remote, port), local_port)
            .is_err()
        {
            return NetResult::Error;
        }
        let smol = self.sockets.add(socket);
        let handle = self.fresh();
        self.table.insert(handle, Entry::Tcp(smol));
        self.drive();
        NetResult::Ready(handle)
    }

    fn connect_check(&mut self, socket: SocketHandle) -> NetResult<()> {
        self.drive();
        let outcome = self.with_grace(|net| {
            let Some(Entry::Tcp(smol)) = net.table.get(&socket) else {
                return Some(NetResult::Error);
            };
            match net.sockets.get::<tcp::Socket>(*smol).state() {
                tcp::State::Established | tcp::State::CloseWait => Some(NetResult::Ready(())),
                tcp::State::SynSent | tcp::State::SynReceived => None,
                _ => Some(NetResult::Error),
            }
        });
        outcome.unwrap_or(NetResult::WouldBlock)
    }

    fn tcp_listen(&mut self, addr: &[u8], port: u16, _backlog: i32) -> NetResult<SocketHandle> {
        let Some(bind_addr) = ip_from_bytes(addr) else {
            return NetResult::Error;
        };
        let port = if port == 0 { self.ephemeral_port() } else { port };
        let endpoint = IpListenEndpoint {
            addr: Some(bind_addr).filter(|ip| !ip.is_unspecified()),
            port,
        };
        let rx = tcp::SocketBuffer::new(vec![0u8; self.config.tcp_rx_buffer]);
        let tx = tcp::SocketBuffer::new(vec![0u8; self.config.tcp_tx_buffer]);
        let mut first = tcp::Socket::new(rx, tx);
        if first.listen(endpoint).is_err() {
            return NetResult::Error;
        }
        let pool = vec![self.sockets.add(first)];
        let handle = self.fresh();
        self.table.insert(handle, Entry::Listener { endpoint, pool });
        self.drive();
        NetResult::Ready(handle)
    }

    fn accept(&mut self, listener: SocketHandle) -> NetResult<SocketHandle> {
        self.drive();
        let taken = self.with_grace(|net| {
            let Some(Entry::Listener { pool, .. }) = net.table.get_mut(&listener) else {
                return Some(Err(()));
            };
            let mut found = None;
            for (index, &smol) in pool.iter().enumerate() {
                if Self::acceptable(net.sockets.get::<tcp::Socket>(smol).state()) {
                    found = Some(index);
                    break;
                }
            }
            found.map(|index| Ok(pool.swap_remove(index)))
        });
        match taken {
            Some(Ok(smol)) => {
                let handle = self.fresh();
                self.table.insert(handle, Entry::Tcp(smol));
                self.drive();
                NetResult::Ready(handle)
            }
            Some(Err(())) => NetResult::Error,
            None => NetResult::WouldBlock,
        }
    }

    fn recv(&mut self, socket: SocketHandle, buf: &mut [u8]) -> NetResult<usize> {
        self.drive();
        let outcome = self.with_grace(|net| {
            let Some(Entry::Tcp(smol)) = net.table.get(&socket) else {
                return Some(NetResult::Error);
            };
            match net.sockets.get_mut::<tcp::Socket>(*smol).recv_slice(buf) {
                Ok(0) if !buf.is_empty() => None,
                Ok(n) => Some(NetResult::Ready(n)),
                Err(tcp::RecvError::Finished) => Some(NetResult::Ready(0)),
                Err(tcp::RecvError::InvalidState) => Some(NetResult::Error),
            }
        });
        outcome.unwrap_or(NetResult::WouldBlock)
    }

    fn send(&mut self, socket: SocketHandle, buf: &[u8]) -> NetResult<usize> {
        let outcome = self.with_grace(|net| {
            let Some(Entry::Tcp(smol)) = net.table.get(&socket) else {
                return Some(NetResult::Error);
            };
            match net.sockets.get_mut::<tcp::Socket>(*smol).send_slice(buf) {
                Ok(0) if !buf.is_empty() => None,
                Ok(n) => Some(NetResult::Ready(n)),
                Err(tcp::SendError::InvalidState) => Some(NetResult::Error),
            }
        });
        self.drive();
        outcome.unwrap_or(NetResult::WouldBlock)
    }

    fn udp_bind(&mut self, addr: &[u8], port: u16) -> NetResult<SocketHandle> {
        let Some(bind_addr) = ip_from_bytes(addr) else {
            return NetResult::Error;
        };
        let port = if port == 0 { self.ephemeral_port() } else { port };
        let rx = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; self.config.udp_slots],
            vec![0u8; self.config.udp_buffer],
        );
        let tx = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; self.config.udp_slots],
            vec![0u8; self.config.udp_buffer],
        );
        let mut socket = udp::Socket::new(rx, tx);
        let endpoint = IpListenEndpoint {
            addr: Some(bind_addr).filter(|ip| !ip.is_unspecified()),
            port,
        };
        if socket.bind(endpoint).is_err() {
            return NetResult::Error;
        }
        let smol = self.sockets.add(socket);
        let handle = self.fresh();
        self.table.insert(handle, Entry::Udp(smol));
        NetResult::Ready(handle)
    }

    fn udp_send_to(
        &mut self,
        socket: SocketHandle,
        buf: &[u8],
        addr: &[u8],
        port: u16,
    ) -> NetResult<usize> {
        let Some(remote) = ip_from_bytes(addr) else {
            return NetResult::Error;
        };
        let Some(Entry::Udp(smol)) = self.table.get(&socket) else {
            return NetResult::Error;
        };
        let result = match self
            .sockets
            .get_mut::<udp::Socket>(*smol)
            .send_slice(buf, IpEndpoint::new(remote, port))
        {
            Ok(()) => NetResult::Ready(buf.len()),
            Err(udp::SendError::BufferFull) => NetResult::WouldBlock,
            Err(udp::SendError::Unaddressable) => NetResult::Error,
        };
        self.drive();
        result
    }

    fn udp_recv_from(
        &mut self,
        socket: SocketHandle,
        buf: &mut [u8],
        sender_addr: &mut [u8],
    ) -> NetResult<(usize, usize, u16)> {
        self.drive();
        let outcome = self.with_grace(|net| {
            let Some(Entry::Udp(smol)) = net.table.get(&socket) else {
                return Some(NetResult::Error);
            };
            match net.sockets.get_mut::<udp::Socket>(*smol).recv() {
                Ok((payload, meta)) => {
                    let n = payload.len().min(buf.len());
                    buf[..n].copy_from_slice(&payload[..n]);
                    let bytes = addr_bytes(&meta.endpoint.addr);
                    let len = bytes.len().min(sender_addr.len());
                    sender_addr[..len].copy_from_slice(&bytes[..len]);
                    Some(NetResult::Ready((n, len, meta.endpoint.port)))
                }
                Err(udp::RecvError::Exhausted) => None,
                Err(udp::RecvError::Truncated) => Some(NetResult::Error),
            }
        });
        outcome.unwrap_or(NetResult::WouldBlock)
    }

    fn local_port(&mut self, socket: SocketHandle) -> Option<u16> {
        match self.table.get(&socket)? {
            Entry::Tcp(smol) => {
                self.sockets.get::<tcp::Socket>(*smol).local_endpoint().map(|e| e.port)
            }
            Entry::Listener { endpoint, .. } => Some(endpoint.port),
            Entry::Udp(smol) => {
                let port = self.sockets.get::<udp::Socket>(*smol).endpoint().port;
                (port != 0).then_some(port)
            }
        }
    }

    fn close(&mut self, socket: SocketHandle) {
        self.armed.remove(&socket);
        let Some(entry) = self.table.remove(&socket) else { return };
        match entry {
            Entry::Tcp(smol) => {
                self.sockets.get_mut::<tcp::Socket>(smol).close();
                self.closing.push(smol);
            }
            Entry::Listener { pool, .. } => {
                for smol in pool {
                    self.sockets.get_mut::<tcp::Socket>(smol).close();
                    self.closing.push(smol);
                }
            }
            Entry::Udp(smol) => {
                self.sockets.get_mut::<udp::Socket>(smol).close();
                self.sockets.remove(smol);
            }
        }
        self.drive();
    }

    fn register(&mut self, socket: SocketHandle, interest: Interest) {
        self.armed.insert(socket, interest);
    }

    fn deregister(&mut self, socket: SocketHandle) {
        self.armed.remove(&socket);
    }

    fn poll(&mut self, timeout_ms: Option<u64>) -> Vec<SocketHandle> {
        let limit = timeout_ms.unwrap_or(self.config.max_block_ms);
        let start = (self.now_ms)();
        loop {
            self.drive();
            let ready = self.ready_armed();
            if !ready.is_empty() {
                return ready;
            }
            if (self.now_ms)().saturating_sub(start) >= limit {
                return Vec::new();
            }
        }
    }

    fn network_available(&mut self) -> bool {
        self.drive();
        self.iface.ipv4_addr().is_some()
    }

    fn interface_count(&mut self) -> u32 {
        1
    }

    fn interface_info(&mut self, index: u32) -> Option<InterfaceInfo> {
        if index != 0 {
            return None;
        }
        let ipv4 = self.iface.ipv4_addr().map(|addr| addr.octets());
        let subnet = self
            .iface
            .ip_addrs()
            .first()
            .map_or([0, 0, 0, 0], |cidr| prefix_to_mask(cidr.prefix_len()));
        Some(InterfaceInfo {
            oper_status: if ipv4.is_some() {
                OperStatus::Up
            } else if self.dhcp.is_some() {
                OperStatus::Dormant
            } else {
                OperStatus::Down
            },
            kind: IfaceKind::Ethernet,
            ipv4: ipv4.unwrap_or([0, 0, 0, 0]),
            subnet,
            gateway: self.gateway.unwrap_or([0, 0, 0, 0]),
            dhcp_enabled: self.dhcp.is_some(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};

    /// The AOT loopback tier's clock stand-in: every read advances one "millisecond"
    /// (`lamella-runtime-support-net`'s `counting_now_ms`), so the in-op grace window
    /// always makes progress in a bounded number of calls.
    static TICKS: AtomicU64 = AtomicU64::new(0);
    fn counting_now_ms() -> u64 {
        TICKS.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The scheduler-less single-thread loopback the AOT QEMU e2e runs
    /// (`lamella_net_support_init_loopback`): smoltcp's in-memory device at 127.0.0.1/8,
    /// the counting clock, and the 50 ms in-op grace.
    fn loopback_net() -> SmoltcpNet<smoltcp::phy::Loopback> {
        let device = smoltcp::phy::Loopback::new(smoltcp::phy::Medium::Ethernet);
        let mut config = NetConfig::new(
            [0x02, 0, 0, 0, 0, 0x01],
            IpSetup::Static {
                addr: [127, 0, 0, 1],
                prefix_len: 8,
                gateway: None,
                dns: Vec::new(),
            },
        );
        config.would_block_grace_ms = 50;
        SmoltcpNet::new(device, config, counting_now_ms, 0x6c61_6d65_6c6c_6100)
    }

    #[test]
    fn interface_info_reports_the_static_loopback_config() {
        let mut net = loopback_net();
        assert!(net.network_available());
        assert_eq!(net.interface_count(), 1);
        let info = net.interface_info(0).expect("interface 0");
        assert_eq!(info.ipv4, [127, 0, 0, 1]);
        assert_eq!(info.subnet, [255, 0, 0, 0]);
        assert_eq!(info.oper_status, OperStatus::Up);
        assert_eq!(info.kind, IfaceKind::Ethernet);
        assert_eq!(info.gateway, [0, 0, 0, 0]);
        assert!(!info.dhcp_enabled);
        assert!(net.interface_info(1).is_none());
    }

    /// Busy-retries `operation` like the managed poll loops (`Socket.cs` spins on the
    /// WouldBlock sentinel), bounded so a stack that stops progressing fails the test
    /// instead of hanging it.
    fn busy_poll<T>(what: &str, mut operation: impl FnMut() -> NetResult<T>) -> T {
        for _ in 0..10_000 {
            match operation() {
                NetResult::Ready(value) => return value,
                NetResult::WouldBlock => {}
                NetResult::Error => panic!("{what}: error"),
            }
        }
        panic!("{what}: still would-block after 10000 retries");
    }

    /// The whole-managed-Socket QEMU e2e's exact shape, on the host: bind+listen,
    /// connect, accept, `client.Send({40, 2})` -> `accepted.Receive(buf)` == 42's worth
    /// of payload. One single-threaded stack; every wait is the seam's own in-op grace.
    #[test]
    fn loopback_moves_bytes_from_client_to_accepted_socket() {
        let mut net = loopback_net();
        let listener = busy_poll("listen", || net.tcp_listen(&[0, 0, 0, 0], 4242, 2));
        let client = busy_poll("connect-start", || net.tcp_connect(&[127, 0, 0, 1], 4242));
        busy_poll("connect", || net.connect_check(client));
        let accepted = busy_poll("accept", || net.accept(listener));

        let sent = busy_poll("send", || net.send(client, &[40, 2]));
        assert_eq!(sent, 2, "the client queues the whole payload");
        let mut buf = [0u8; 8];
        let received = busy_poll("receive", || net.recv(accepted, &mut buf));
        assert_eq!(received, 2, "the payload crosses the loopback");
        assert_eq!(&buf[..2], &[40, 2]);
    }
}
