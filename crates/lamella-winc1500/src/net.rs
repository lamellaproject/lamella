//! The [`NetBackend`] implementation over the WINC's on-module socket stack: every seam op maps
//! onto one socket-group HIF command, and the module's IRQ-signalled replies drive the readiness
//! reactor. The embedder brings the module up first ([`crate::Winc1500::start`], firmware boot,
//! Wi-Fi join, DHCP); this backend then owns the SOCKET group -- the association outlives any
//! one backend instance, so a fresh backend per evaluation drives the same joined module.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use lamella_net_core::{Interest, NetBackend, NetResult, SocketHandle};

use crate::WincControl;
use crate::hif::{self, HifError, HifEvent, ModuleBus};
use crate::socket::{self, AcceptReply, ConnectReply, RecvReply, SendReply, SockAddr, StatusReply};

/// The number of module socket slots (TCP 0..6, UDP 7..10).
const SOCK_SLOTS: usize = 11;
/// The most events one pump pass consumes -- a bound so a socket op never disappears into
/// event servicing (the reactor's poll loops passes anyway).
const PUMP_BATCH: usize = 8;
/// The receive command's firmware-side window, milliseconds. FINITE deliberately: the proven
/// on-air receives all posted finite windows, while an "infinite" 0xFFFFFFFF is untested
/// against this firmware -- and a timeout status is benign anyway (the next receive re-posts).
const RECV_TIMEOUT_MS: u32 = 60_000;
/// The first pseudo-ephemeral local port for port-0 binds (the module reports no ephemeral
/// choice back, so the host picks explicitly; the IANA dynamic range).
const EPHEMERAL_FIRST: u16 = 49152;

/// Construction-time knobs.
#[derive(Clone, Debug)]
pub struct WincNetConfig {
    /// Per-receive host buffer bytes: the `u16BufLen` cap each posted receive advertises, and
    /// so the most bytes one receive reply carries.
    pub recv_buffer: usize,
    /// The cap applied to [`NetBackend::poll`] with no timeout: a device serve loop must
    /// regain control to answer its own wire, so "block indefinitely" becomes "block this
    /// long, then report nothing ready" (the scheduler simply polls again).
    pub max_block_ms: u64,
    /// How long [`NetBackend::resolve`] drives the module's DNS before reporting failure.
    pub resolve_timeout_ms: u64,
    /// How long a would-block operation keeps pumping in-op before reporting
    /// [`NetResult::WouldBlock`]. 0 = report immediately (the reactor/scheduler shape); a
    /// SCHEDULER-LESS embedder (a device boot-run stepping one session) sets a small grace so
    /// managed busy-retries collapse by orders of magnitude.
    pub would_block_grace_ms: u64,
    /// A bring-up trace hook: every posted command and consumed event narrates one short
    /// line through it (an embedder points it at its console UART). `None` = silent.
    pub trace: Option<fn(&str)>,
}

impl Default for WincNetConfig {
    fn default() -> Self {
        Self {
            recv_buffer: 1024,
            max_block_ms: 100,
            resolve_timeout_ms: 10_000,
            would_block_grace_ms: 0,
            trace: None,
        }
    }
}

/// A connected (or connecting) TCP socket's host-side state.
struct TcpState {
    connected: bool,
    /// The send payload's offset within a HIF message body, from the connect/accept reply
    /// (`u16AppDataOffset - HEADER_LEN`).
    data_offset: u16,
    send_in_flight: bool,
    recv_posted: bool,
    /// Received-and-not-yet-drained bytes (one receive reply's payload), with the drain cursor.
    buf: Vec<u8>,
    buf_pos: usize,
    /// The peer closed cleanly (a receive reply with status 0); drained data still reads out,
    /// then recv reports 0.
    peer_closed: bool,
}

impl TcpState {
    fn fresh(connected: bool, data_offset: u16) -> Self {
        Self {
            connected,
            data_offset,
            send_in_flight: false,
            recv_posted: false,
            buf: Vec::new(),
            buf_pos: 0,
            peer_closed: false,
        }
    }

    fn buffered(&self) -> usize {
        self.buf.len() - self.buf_pos
    }
}

/// A listener's host-side state: the bind -> listen chain progress and the accepted-connection
/// queue (unsolicited accept replies land here until `accept` collects them).
struct ListenerState {
    port: u16,
    backlog: u8,
    bound: bool,
    listen_posted: bool,
    pending: VecDeque<SocketHandle>,
}

/// A bound UDP socket's host-side state: whole datagrams queue (boundaries preserved).
struct UdpState {
    port: u16,
    send_in_flight: bool,
    recv_posted: bool,
    datagrams: VecDeque<(SockAddr, Vec<u8>)>,
}

enum Kind {
    Tcp(TcpState),
    Listener(ListenerState),
    Udp(UdpState),
}

/// One open module socket.
struct Open {
    sock: i8,
    session: u16,
    errored: bool,
    kind: Kind,
}

/// What a would-block dwell is waiting to observe.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WaitFor {
    Readable,
    Writable,
    ConnectDone,
}

/// A fully-read, acknowledged module event, ready to apply to host state (bus reads all happen
/// before the acknowledgement; state mutation happens after). Receive replies are absent here:
/// their payloads copy STRAIGHT into the owning socket's retained buffer inside the consume
/// step (a per-event scratch allocation would spend the whole page's size in permanent
/// allocations on a bump-arena embedder).
enum Decoded {
    Connect(ConnectReply),
    Send(SendReply),
    Bind(StatusReply),
    Listen(StatusReply),
    Accept(AcceptReply),
    Dns([u8; 4]),
    WifiDown,
    Other,
}

/// The WINC-backed [`NetBackend`]: the module bus + control pins, the socket table, and the
/// per-socket host state. Dropping it closes every socket it opened, so the module carries no
/// leaked state into the next backend (the next evaluation).
pub struct WincNet<B: ModuleBus, C: WincControl> {
    bus: B,
    ctrl: C,
    now_ms: fn() -> u64,
    config: WincNetConfig,
    table: socket::SocketTable,
    open: BTreeMap<SocketHandle, Open>,
    by_sock: [Option<SocketHandle>; SOCK_SLOTS],
    generation: [u16; SOCK_SLOTS],
    watch: Vec<(SocketHandle, Interest)>,
    /// The last DNS answer ([0;4] = the module reported failure), consumed by `resolve`.
    dns: Option<[u8; 4]>,
    dns_pending: bool,
    /// Listener sockets whose bind confirmed but whose listen command is not yet posted, and
    /// firmware-assigned accept sockets the host could not adopt -- both posted after the
    /// event batch (state mutation stays bus-free).
    defer_listen: Vec<SocketHandle>,
    defer_close: Vec<(i8, u16)>,
    /// One beat separates a receive-done acknowledgement from the next command (transport
    /// pacing the fast SPI needed).
    just_acked: bool,
    /// The IRQN pacing counter: polls between IRQ assertions are skipped, with every 64th
    /// forced as a liveness net.
    idle: u32,
    next_ephemeral: u16,
}

impl<B: ModuleBus, C: WincControl> core::fmt::Debug for WincNet<B, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WincNet").field("open", &self.open.len()).finish_non_exhaustive()
    }
}

impl<B: ModuleBus, C: WincControl> WincNet<B, C> {
    /// Wraps an already-running module link (SPI protocol initialized, firmware booted, Wi-Fi
    /// joined). `now_ms` is the embedder's monotonic millisecond clock -- deadlines only, never
    /// slept on directly (waits pace through [`WincControl::delay_ms`]).
    pub fn new(bus: B, ctrl: C, now_ms: fn() -> u64, config: WincNetConfig) -> Self {
        Self {
            bus,
            ctrl,
            now_ms,
            config,
            table: socket::SocketTable::new(),
            open: BTreeMap::new(),
            by_sock: [None; SOCK_SLOTS],
            generation: [0; SOCK_SLOTS],
            watch: Vec::new(),
            dns: None,
            dns_pending: false,
            defer_listen: Vec::new(),
            defer_close: Vec::new(),
            just_acked: false,
            idle: 0,
            next_ephemeral: EPHEMERAL_FIRST,
        }
    }

    fn handle_for(&self, sock: i8) -> SocketHandle {
        (u32::from(self.generation[sock as usize]) << 8) | (sock as u8 as u32)
    }


    /// Consumes up to one event batch (non-blocking), then posts any follow-up commands the
    /// batch deferred (a confirmed bind's listen; a close for an unadoptable accept).
    fn pump_now(&mut self) {
        for _ in 0..PUMP_BATCH {
            match hif::poll_event(&mut self.bus) {
                Ok(Some(event)) => {
                    let decoded = self.consume(event);
                    self.apply(decoded);
                }
                Ok(None) | Err(_) => break,
            }
        }
        self.flush_deferred();
    }

    /// The IRQN-paced poll gate for dwell loops: `true` when the caller should pump now.
    /// Otherwise idles one millisecond (with every 64th check forced, a liveness net).
    fn pump_due(&mut self) -> bool {
        if self.ctrl.irq_asserted() {
            self.idle = 0;
            return true;
        }
        self.idle += 1;
        if self.idle >= 64 {
            self.idle = 0;
            return true;
        }
        self.ctrl.delay_ms(1);
        false
    }

    /// Reads an event's payload, acknowledges the slot, and returns the decoded reply. The
    /// acknowledgement is unconditional -- a failed read still releases the slot (holding it
    /// starves every event after it), and the lost event surfaces as that operation's timeout
    /// rather than a wedged module.
    fn consume(&mut self, event: HifEvent) -> Decoded {
        let payload = event.address + u32::from(hif::HEADER_LEN);
        let decoded = if event.group == hif::GROUP_IP {
            match event.opcode {
                socket::CMD_CONNECT | socket::CMD_SSL_CONNECT => {
                    let mut raw = [0u8; 4];
                    match self.bus.read_block(payload, &mut raw) {
                        Ok(()) => Decoded::Connect(ConnectReply::decode(&raw)),
                        Err(_) => Decoded::Other,
                    }
                }
                socket::CMD_SEND | socket::CMD_SSL_SEND | socket::CMD_SENDTO => {
                    let mut raw = [0u8; 8];
                    match self.bus.read_block(payload, &mut raw) {
                        Ok(()) => Decoded::Send(SendReply::decode(&raw)),
                        Err(_) => Decoded::Other,
                    }
                }
                socket::CMD_RECV | socket::CMD_SSL_RECV | socket::CMD_RECVFROM => {
                    let mut raw = [0u8; 16];
                    match self.bus.read_block(payload, &mut raw) {
                        Ok(()) => {
                            self.consume_recv(payload, RecvReply::decode(&raw));
                            Decoded::Other
                        }
                        Err(_) => Decoded::Other,
                    }
                }
                socket::CMD_BIND => {
                    let mut raw = [0u8; 4];
                    match self.bus.read_block(payload, &mut raw) {
                        Ok(()) => Decoded::Bind(StatusReply::decode(&raw)),
                        Err(_) => Decoded::Other,
                    }
                }
                socket::CMD_LISTEN => {
                    let mut raw = [0u8; 4];
                    match self.bus.read_block(payload, &mut raw) {
                        Ok(()) => Decoded::Listen(StatusReply::decode(&raw)),
                        Err(_) => Decoded::Other,
                    }
                }
                socket::CMD_ACCEPT => {
                    let mut raw = [0u8; 12];
                    match self.bus.read_block(payload, &mut raw) {
                        Ok(()) => Decoded::Accept(AcceptReply::decode(&raw)),
                        Err(_) => Decoded::Other,
                    }
                }
                socket::CMD_DNS_RESOLVE => {
                    let mut raw = [0u8; 68];
                    match self.bus.read_block(payload, &mut raw) {
                        Ok(()) => Decoded::Dns(socket::dns_reply_ip(&raw)),
                        Err(_) => Decoded::Other,
                    }
                }
                _ => Decoded::Other,
            }
        } else if event.group == hif::GROUP_WIFI
            && event.opcode == crate::wifi::RESP_CON_STATE_CHANGED
        {
            let mut raw = [0u8; 4];
            match self.bus.read_block(payload, &mut raw) {
                Ok(()) if raw[0] != 1 => Decoded::WifiDown,
                _ => Decoded::Other,
            }
        } else {
            Decoded::Other
        };
        let _ = hif::set_receive_done(&mut self.bus);
        self.just_acked = true;
        if let Some(trace) = self.config.trace {
            trace(&alloc::format!("[winc] evt {}/{:#04x} len {}", event.group, event.opcode, event.length));
        }
        decoded
    }

    /// Applies a decoded reply to host state. Bus-free: follow-up commands are deferred to
    /// [`flush_deferred`](Self::flush_deferred).
    fn apply(&mut self, decoded: Decoded) {
        if let Some(trace) = self.config.trace {
            match &decoded {
                Decoded::Connect(reply) => trace(&alloc::format!(
                    "[winc] connect sock {} error {} offset {}",
                    reply.sock, reply.error, reply.app_data_offset
                )),
                Decoded::Send(reply) => trace(&alloc::format!(
                    "[winc] send sock {} accepted {}",
                    reply.sock, reply.sent
                )),
                _ => {}
            }
        }
        match decoded {
            Decoded::Connect(reply) => {
                if let Some(open) = self.open_by_sock(reply.sock) {
                    if let Kind::Tcp(tcp) = &mut open.kind {
                        if reply.error == 0 {
                            tcp.connected = true;
                            tcp.data_offset = reply.app_data_offset.saturating_sub(hif::HEADER_LEN);
                        } else {
                            open.errored = true;
                        }
                    }
                }
            }
            Decoded::Send(reply) => {
                if let Some(open) = self.open_by_sock(reply.sock) {
                    match &mut open.kind {
                        Kind::Tcp(tcp) => tcp.send_in_flight = false,
                        Kind::Udp(udp) => udp.send_in_flight = false,
                        Kind::Listener(_) => {}
                    }
                    if reply.sent < 0 {
                        open.errored = true;
                    }
                }
            }
            Decoded::Bind(reply) => {
                if !(0..SOCK_SLOTS as i8).contains(&reply.sock) {
                    return;
                }
                let Some(handle) = self.by_sock[reply.sock as usize] else {
                    return;
                };
                let mut bound_listener = false;
                if let Some(open) = self.open.get_mut(&handle) {
                    if reply.status != 0 {
                        open.errored = true;
                    } else if let Kind::Listener(listener) = &mut open.kind {
                        listener.bound = true;
                        bound_listener = true;
                    }
                }
                if bound_listener {
                    self.defer_listen.push(handle);
                }
            }
            Decoded::Listen(reply) => {
                if let Some(open) = self.open_by_sock(reply.sock) {
                    if reply.status != 0 {
                        open.errored = true;
                    }
                }
            }
            Decoded::Accept(reply) => {
                let Some(listener_handle) = self
                    .by_sock
                    .get(reply.listen_sock as usize)
                    .copied()
                    .flatten()
                else {
                    self.defer_close.push((reply.connected_sock, 0));
                    return;
                };
                if !self.table.claim_specific(reply.connected_sock) {
                    self.defer_close.push((reply.connected_sock, 0));
                    return;
                }
                let session = self.table.next_session();
                let handle = self.handle_for(reply.connected_sock);
                self.open.insert(
                    handle,
                    Open {
                        sock: reply.connected_sock,
                        session,
                        errored: false,
                        kind: Kind::Tcp(TcpState::fresh(
                            true,
                            reply.app_data_offset.saturating_sub(hif::HEADER_LEN),
                        )),
                    },
                );
                self.by_sock[reply.connected_sock as usize] = Some(handle);
                let queued = match self.open.get_mut(&listener_handle) {
                    Some(Open { kind: Kind::Listener(listener), .. }) => {
                        listener.pending.push_back(handle);
                        true
                    }
                    _ => false,
                };
                if !queued {
                    self.open.remove(&handle);
                    self.by_sock[reply.connected_sock as usize] = None;
                    self.table.release(reply.connected_sock);
                    self.defer_close.push((reply.connected_sock, session));
                }
            }
            Decoded::Dns(ip) => {
                self.dns = Some(ip);
                self.dns_pending = false;
            }
            Decoded::WifiDown => {
                for open in self.open.values_mut() {
                    open.errored = true;
                }
            }
            Decoded::Other => {}
        }
    }

    fn open_by_sock(&mut self, sock: i8) -> Option<&mut Open> {
        if !(0..SOCK_SLOTS as i8).contains(&sock) {
            return None;
        }
        let handle = self.by_sock[sock as usize]?;
        self.open.get_mut(&handle)
    }

    /// Consumes one receive reply IN PLACE: a TCP payload copies straight into the owning
    /// socket's retained buffer, whose capacity persists across drains -- so a whole page
    /// costs one allocation per socket, not one per chunk (the bump-arena economy that
    /// exhausted a 16 KiB arena when every chunk allocated fresh). Runs before the slot
    /// acknowledgement; field-path borrows keep the bus and the socket table independent.
    fn consume_recv(&mut self, payload: u32, reply: RecvReply) {
        if let Some(trace) = self.config.trace {
            trace(&alloc::format!(
                "[winc] recv sock {} status {} off {} session {}",
                reply.sock, reply.status, reply.data_offset, reply.session
            ));
        }
        if !(0..SOCK_SLOTS as i8).contains(&reply.sock) {
            return;
        }
        let Some(handle) = self.by_sock[reply.sock as usize] else {
            return;
        };
        let Some(open) = self.open.get_mut(&handle) else {
            return;
        };
        if reply.session != open.session {
            return;
        }
        let take = (reply.status.max(0) as usize)
            .min(usize::from(hif::MAX_MESSAGE))
            .min(self.config.recv_buffer);
        match &mut open.kind {
            Kind::Tcp(tcp) => {
                tcp.recv_posted = false;
                if reply.status > 0 {
                    if tcp.buffered() == 0 {
                        tcp.buf.clear();
                        tcp.buf_pos = 0;
                    }
                    let start = tcp.buf.len();
                    tcp.buf.resize(start + take, 0);
                    if self
                        .bus
                        .read_block(payload + u32::from(reply.data_offset), &mut tcp.buf[start..])
                        .is_err()
                    {
                        tcp.buf.truncate(start);
                    }
                } else if reply.status == 0 {
                    tcp.peer_closed = true;
                } else if reply.status == socket::ERR_TIMEOUT {
                } else {
                    open.errored = true;
                }
            }
            Kind::Udp(udp) => {
                udp.recv_posted = false;
                if reply.status >= 0 {
                    let mut data = alloc::vec![0u8; take];
                    if take > 0
                        && self
                            .bus
                            .read_block(payload + u32::from(reply.data_offset), &mut data)
                            .is_err()
                    {
                        data.clear();
                    }
                    udp.datagrams.push_back((reply.remote, data));
                } else {
                    open.errored = true;
                }
            }
            Kind::Listener(_) => {}
        }
    }

    /// Posts the commands an event batch deferred.
    fn flush_deferred(&mut self) {
        while let Some(handle) = self.defer_listen.pop() {
            let Some(Open { sock, session, kind: Kind::Listener(listener), .. }) =
                self.open.get_mut(&handle)
            else {
                continue;
            };
            if listener.listen_posted {
                continue;
            }
            listener.listen_posted = true;
            let cmd = socket::listen_cmd(*sock, listener.backlog, *session);
            if self.post(socket::CMD_LISTEN, &cmd, None).is_err() {
                if let Some(open) = self.open.get_mut(&handle) {
                    open.errored = true;
                }
            }
        }
        while let Some((sock, session)) = self.defer_close.pop() {
            let cmd = socket::close_cmd(sock, session);
            let _ = self.post(socket::CMD_CLOSE, &cmd, None);
        }
    }

    /// One HIF command to the socket group, with the ack-to-command settle beat the fast
    /// transport needs.
    fn post(&mut self, opcode: u8, ctrl: &[u8], data: Option<(&[u8], u16)>) -> Result<(), HifError> {
        if self.just_acked {
            self.ctrl.delay_ms(1);
            self.just_acked = false;
        }
        let result = hif::send(&mut self.bus, hif::GROUP_IP, opcode, ctrl, data);
        if let Some(trace) = self.config.trace {
            trace(&alloc::format!(
                "[winc] cmd {:#04x} {}",
                opcode,
                if result.is_ok() { "ok" } else { "FAILED" }
            ));
        }
        result
    }


    fn wait_satisfied(&self, handle: SocketHandle, what: WaitFor) -> bool {
        let Some(open) = self.open.get(&handle) else {
            return true;
        };
        if open.errored {
            return true;
        }
        match (&open.kind, what) {
            (Kind::Tcp(tcp), WaitFor::Readable) => tcp.buffered() > 0 || tcp.peer_closed,
            (Kind::Tcp(tcp), WaitFor::Writable) => tcp.connected && !tcp.send_in_flight,
            (Kind::Tcp(tcp), WaitFor::ConnectDone) => tcp.connected,
            (Kind::Listener(listener), WaitFor::Readable) => !listener.pending.is_empty(),
            (Kind::Listener(_), _) => false,
            (Kind::Udp(udp), WaitFor::Readable) => !udp.datagrams.is_empty(),
            (Kind::Udp(udp), WaitFor::Writable) => !udp.send_in_flight,
            (Kind::Udp(_), WaitFor::ConnectDone) => true,
        }
    }

    /// Keeps pumping for up to the would-block grace (0 = no dwell) waiting for `what`.
    /// Returns whether it was observed.
    fn dwell(&mut self, handle: SocketHandle, what: WaitFor) -> bool {
        if self.wait_satisfied(handle, what) {
            return true;
        }
        let grace = self.config.would_block_grace_ms;
        if grace == 0 {
            return false;
        }
        let deadline = (self.now_ms)().saturating_add(grace);
        loop {
            if self.pump_due() {
                self.pump_now();
                if self.wait_satisfied(handle, what) {
                    return true;
                }
            }
            if (self.now_ms)() >= deadline {
                return false;
            }
        }
    }


    /// Posts the one outstanding receive a socket may have (TCP stream or UDP datagram form).
    fn post_recv(&mut self, handle: SocketHandle) -> Result<(), HifError> {
        let Some(open) = self.open.get_mut(&handle) else {
            return Ok(());
        };
        let sock = open.sock;
        let session = open.session;
        let cap = self.config.recv_buffer.min(u16::MAX as usize) as u16;
        let (posted, opcode) = match &mut open.kind {
            Kind::Tcp(tcp) => (&mut tcp.recv_posted, socket::CMD_RECV),
            Kind::Udp(udp) => (&mut udp.recv_posted, socket::CMD_RECVFROM),
            Kind::Listener(_) => return Ok(()),
        };
        if *posted {
            return Ok(());
        }
        *posted = true;
        let cmd = socket::recv_cmd(sock, RECV_TIMEOUT_MS, session, cap);
        let result = self.post(opcode, &cmd, None);
        if result.is_err() {
            if let Some(open) = self.open.get_mut(&handle) {
                open.errored = true;
            }
        }
        result
    }

    fn alloc_ephemeral(&mut self) -> u16 {
        let port = self.next_ephemeral;
        self.next_ephemeral = if self.next_ephemeral == u16::MAX {
            EPHEMERAL_FIRST
        } else {
            self.next_ephemeral + 1
        };
        port
    }

    fn close_all(&mut self) {
        let handles: Vec<SocketHandle> = self.open.keys().copied().collect();
        for handle in handles {
            self.close_handle(handle);
        }
    }

    fn close_handle(&mut self, handle: SocketHandle) {
        let Some(open) = self.open.remove(&handle) else {
            return;
        };
        if let Kind::Listener(listener) = &open.kind {
            let pending: Vec<SocketHandle> = listener.pending.iter().copied().collect();
            for accepted in pending {
                self.close_handle(accepted);
            }
        }
        let cmd = socket::close_cmd(open.sock, open.session);
        let _ = self.post(socket::CMD_CLOSE, &cmd, None);
        let index = open.sock as usize;
        self.by_sock[index] = None;
        self.generation[index] = self.generation[index].wrapping_add(1);
        self.table.release(open.sock);
        self.watch.retain(|(watched, _)| *watched != handle);
    }

    fn addr4(addr: &[u8]) -> Option<[u8; 4]> {
        if addr.len() == 4 { Some([addr[0], addr[1], addr[2], addr[3]]) } else { None }
    }
}

impl<B: ModuleBus, C: WincControl> NetBackend for WincNet<B, C> {
    fn resolve(&mut self, host: &str) -> Vec<Vec<u8>> {
        if let Ok(ip) = host.parse::<core::net::Ipv4Addr>() {
            return alloc::vec![ip.octets().to_vec()];
        }
        if host.is_empty() || host.len() + 1 > socket::HOSTNAME_MAX {
            return Vec::new();
        }
        self.pump_now();
        let mut name = Vec::with_capacity(host.len() + 1);
        name.extend_from_slice(host.as_bytes());
        name.push(0);
        self.dns = None;
        self.dns_pending = true;
        if self.post(socket::CMD_DNS_RESOLVE, &name, None).is_err() {
            self.dns_pending = false;
            return Vec::new();
        }
        let deadline = (self.now_ms)().saturating_add(self.config.resolve_timeout_ms);
        while self.dns_pending && (self.now_ms)() < deadline {
            if self.pump_due() {
                self.pump_now();
            }
        }
        self.dns_pending = false;
        match self.dns.take() {
            Some(ip) if ip != [0; 4] => alloc::vec![ip.to_vec()],
            _ => Vec::new(),
        }
    }

    fn tcp_connect(&mut self, addr: &[u8], port: u16) -> NetResult<SocketHandle> {
        let Some(ip) = Self::addr4(addr) else {
            return NetResult::Error;
        };
        self.pump_now();
        let Some(sock) = self.table.claim_tcp() else {
            return NetResult::Error;
        };
        let session = self.table.next_session();
        let cmd = socket::connect_cmd(sock, &SockAddr { port, ip }, 0, session);
        if self.post(socket::CMD_CONNECT, &cmd, None).is_err() {
            self.table.release(sock);
            return NetResult::Error;
        }
        let handle = self.handle_for(sock);
        self.open.insert(
            handle,
            Open {
                sock,
                session,
                errored: false,
                kind: Kind::Tcp(TcpState::fresh(false, socket::TCP_TX_OFFSET)),
            },
        );
        self.by_sock[sock as usize] = Some(handle);
        NetResult::Ready(handle)
    }

    fn connect_check(&mut self, socket: SocketHandle) -> NetResult<()> {
        self.pump_now();
        self.dwell(socket, WaitFor::ConnectDone);
        let Some(open) = self.open.get(&socket) else {
            return NetResult::Error;
        };
        if open.errored {
            return NetResult::Error;
        }
        match &open.kind {
            Kind::Tcp(tcp) if tcp.connected => NetResult::Ready(()),
            Kind::Tcp(_) => NetResult::WouldBlock,
            _ => NetResult::Error,
        }
    }

    fn tcp_listen(&mut self, addr: &[u8], port: u16, backlog: i32) -> NetResult<SocketHandle> {
        let Some(ip) = Self::addr4(addr) else {
            return NetResult::Error;
        };
        self.pump_now();
        let Some(sock) = self.table.claim_tcp() else {
            return NetResult::Error;
        };
        let session = self.table.next_session();
        let port = if port == 0 { self.alloc_ephemeral() } else { port };
        let cmd = socket::bind_cmd(sock, &SockAddr { port, ip }, session);
        if self.post(socket::CMD_BIND, &cmd, None).is_err() {
            self.table.release(sock);
            return NetResult::Error;
        }
        let handle = self.handle_for(sock);
        self.open.insert(
            handle,
            Open {
                sock,
                session,
                errored: false,
                kind: Kind::Listener(ListenerState {
                    port,
                    backlog: backlog.clamp(1, 255) as u8,
                    bound: false,
                    listen_posted: false,
                    pending: VecDeque::new(),
                }),
            },
        );
        self.by_sock[sock as usize] = Some(handle);
        NetResult::Ready(handle)
    }

    fn accept(&mut self, listener: SocketHandle) -> NetResult<SocketHandle> {
        self.pump_now();
        self.dwell(listener, WaitFor::Readable);
        let Some(open) = self.open.get_mut(&listener) else {
            return NetResult::Error;
        };
        if open.errored {
            return NetResult::Error;
        }
        match &mut open.kind {
            Kind::Listener(state) => match state.pending.pop_front() {
                Some(handle) => NetResult::Ready(handle),
                None => NetResult::WouldBlock,
            },
            _ => NetResult::Error,
        }
    }

    fn recv(&mut self, socket: SocketHandle, buf: &mut [u8]) -> NetResult<usize> {
        if buf.is_empty() {
            return NetResult::Ready(0);
        }
        self.pump_now();
        for pass in 0..2 {
            let Some(open) = self.open.get_mut(&socket) else {
                return NetResult::Error;
            };
            let Kind::Tcp(tcp) = &mut open.kind else {
                return NetResult::Error;
            };
            if tcp.buffered() > 0 {
                let n = tcp.buffered().min(buf.len());
                buf[..n].copy_from_slice(&tcp.buf[tcp.buf_pos..tcp.buf_pos + n]);
                tcp.buf_pos += n;
                if tcp.buffered() == 0 {
                    tcp.buf.clear();
                    tcp.buf_pos = 0;
                }
                return NetResult::Ready(n);
            }
            if open.errored {
                return NetResult::Error;
            }
            let Kind::Tcp(tcp) = &open.kind else {
                return NetResult::Error;
            };
            if tcp.peer_closed {
                return NetResult::Ready(0);
            }
            if !tcp.connected {
                return NetResult::Error;
            }
            let send_outstanding = tcp.send_in_flight;
            if pass == 0 {
                if send_outstanding && !self.dwell(socket, WaitFor::Writable) {
                    break;
                }
                if self.post_recv(socket).is_err() {
                    return NetResult::Error;
                }
                if !self.dwell(socket, WaitFor::Readable) {
                    break;
                }
            }
        }
        NetResult::WouldBlock
    }

    fn send(&mut self, socket: SocketHandle, buf: &[u8]) -> NetResult<usize> {
        if buf.is_empty() {
            return NetResult::Ready(0);
        }
        self.pump_now();
        if !self.dwell(socket, WaitFor::Writable) {
            return NetResult::WouldBlock;
        }
        let Some(open) = self.open.get(&socket) else {
            return NetResult::Error;
        };
        if open.errored {
            return NetResult::Error;
        }
        let Kind::Tcp(tcp) = &open.kind else {
            return NetResult::Error;
        };
        if !tcp.connected {
            return NetResult::Error;
        }
        if tcp.send_in_flight {
            return NetResult::WouldBlock;
        }
        let (sock, session, offset) = (open.sock, open.session, tcp.data_offset);
        let budget = usize::from(hif::MAX_MESSAGE)
            .saturating_sub(usize::from(hif::HEADER_LEN) + usize::from(offset));
        let take = buf.len().min(socket::SEND_MAX).min(budget);
        let cmd = socket::send_cmd(sock, take as u16, None, session);
        if self
            .post(socket::CMD_SEND | hif::OPCODE_DATA_BIT, &cmd, Some((&buf[..take], offset)))
            .is_err()
        {
            if let Some(open) = self.open.get_mut(&socket) {
                open.errored = true;
            }
            return NetResult::Error;
        }
        if let Some(Open { kind: Kind::Tcp(tcp), .. }) = self.open.get_mut(&socket) {
            tcp.send_in_flight = true;
        }
        NetResult::Ready(take)
    }

    fn udp_bind(&mut self, addr: &[u8], port: u16) -> NetResult<SocketHandle> {
        let Some(ip) = Self::addr4(addr) else {
            return NetResult::Error;
        };
        self.pump_now();
        let Some(sock) = self.table.claim_udp() else {
            return NetResult::Error;
        };
        let session = self.table.next_session();
        let port = if port == 0 { self.alloc_ephemeral() } else { port };
        let cmd = socket::bind_cmd(sock, &SockAddr { port, ip }, session);
        if self.post(socket::CMD_BIND, &cmd, None).is_err() {
            self.table.release(sock);
            return NetResult::Error;
        }
        let handle = self.handle_for(sock);
        self.open.insert(
            handle,
            Open {
                sock,
                session,
                errored: false,
                kind: Kind::Udp(UdpState {
                    port,
                    send_in_flight: false,
                    recv_posted: false,
                    datagrams: VecDeque::new(),
                }),
            },
        );
        self.by_sock[sock as usize] = Some(handle);
        NetResult::Ready(handle)
    }

    fn udp_send_to(
        &mut self,
        socket: SocketHandle,
        buf: &[u8],
        addr: &[u8],
        port: u16,
    ) -> NetResult<usize> {
        let Some(ip) = Self::addr4(addr) else {
            return NetResult::Error;
        };
        self.pump_now();
        if !self.dwell(socket, WaitFor::Writable) {
            return NetResult::WouldBlock;
        }
        let Some(open) = self.open.get(&socket) else {
            return NetResult::Error;
        };
        if open.errored {
            return NetResult::Error;
        }
        let Kind::Udp(udp) = &open.kind else {
            return NetResult::Error;
        };
        if udp.send_in_flight {
            return NetResult::WouldBlock;
        }
        let (sock, session) = (open.sock, open.session);
        let budget = usize::from(hif::MAX_MESSAGE)
            .saturating_sub(usize::from(hif::HEADER_LEN) + usize::from(socket::UDP_TX_OFFSET));
        if buf.len() > socket::SEND_MAX.min(budget) {
            return NetResult::Error;
        }
        if buf.is_empty() {
            return NetResult::Ready(0);
        }
        let remote = SockAddr { port, ip };
        let cmd = socket::send_cmd(sock, buf.len() as u16, Some(&remote), session);
        if self
            .post(
                socket::CMD_SENDTO | hif::OPCODE_DATA_BIT,
                &cmd,
                Some((buf, socket::UDP_TX_OFFSET)),
            )
            .is_err()
        {
            if let Some(open) = self.open.get_mut(&socket) {
                open.errored = true;
            }
            return NetResult::Error;
        }
        if let Some(Open { kind: Kind::Udp(udp), .. }) = self.open.get_mut(&socket) {
            udp.send_in_flight = true;
        }
        NetResult::Ready(buf.len())
    }

    fn udp_recv_from(
        &mut self,
        socket: SocketHandle,
        buf: &mut [u8],
        sender_addr: &mut [u8],
    ) -> NetResult<(usize, usize, u16)> {
        self.pump_now();
        for pass in 0..2 {
            let Some(open) = self.open.get_mut(&socket) else {
                return NetResult::Error;
            };
            let errored = open.errored;
            let Kind::Udp(udp) = &mut open.kind else {
                return NetResult::Error;
            };
            if let Some((remote, data)) = udp.datagrams.pop_front() {
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                let addr_len = 4.min(sender_addr.len());
                sender_addr[..addr_len].copy_from_slice(&remote.ip[..addr_len]);
                return NetResult::Ready((n, addr_len, remote.port));
            }
            if errored {
                return NetResult::Error;
            }
            if pass == 0 {
                if self.post_recv(socket).is_err() {
                    return NetResult::Error;
                }
                if !self.dwell(socket, WaitFor::Readable) {
                    break;
                }
            }
        }
        NetResult::WouldBlock
    }

    fn local_port(&mut self, socket: SocketHandle) -> Option<u16> {
        match &self.open.get(&socket)?.kind {
            Kind::Listener(listener) => Some(listener.port),
            Kind::Udp(udp) => Some(udp.port),
            Kind::Tcp(_) => None,
        }
    }

    fn close(&mut self, socket: SocketHandle) {
        self.pump_now();
        self.close_handle(socket);
    }

    fn register(&mut self, socket: SocketHandle, interest: Interest) {
        self.watch.retain(|(handle, existing)| !(*handle == socket && *existing == interest));
        self.watch.push((socket, interest));
    }

    fn deregister(&mut self, socket: SocketHandle) {
        self.watch.retain(|(handle, _)| *handle != socket);
    }

    fn poll(&mut self, timeout_ms: Option<u64>) -> Vec<SocketHandle> {
        let budget = timeout_ms.map_or(self.config.max_block_ms, |timeout| {
            timeout.min(self.config.max_block_ms)
        });
        let deadline = (self.now_ms)().saturating_add(budget);
        self.pump_now();
        loop {
            let mut ready: Vec<SocketHandle> = Vec::new();
            for (handle, interest) in &self.watch {
                let what = match interest {
                    Interest::Read => WaitFor::Readable,
                    Interest::Write => WaitFor::Writable,
                };
                if self.wait_satisfied(*handle, what) && !ready.contains(handle) {
                    ready.push(*handle);
                }
            }
            if !ready.is_empty() {
                return ready;
            }
            if (self.now_ms)() >= deadline {
                return Vec::new();
            }
            if self.pump_due() {
                self.pump_now();
            }
        }
    }
}

impl<B: ModuleBus, C: WincControl> Drop for WincNet<B, C> {
    /// Closes every socket this backend opened, so a dropped evaluation (including a trapped
    /// one) leaves the module's socket table clean for the next backend.
    fn drop(&mut self) {
        self.close_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot::Registers;
    use crate::spi::SpiError;
    use alloc::rc::Rc;
    use alloc::vec;
    use core::cell::RefCell;
    use core::sync::atomic::{AtomicU64, Ordering};

    const CTRL_0: u32 = 0x1070;
    const CTRL_1: u32 = 0x1084;
    const CTRL_2: u32 = 0x1078;
    const CTRL_3: u32 = 0x106c;
    const CTRL_4: u32 = 0x15_0400;

    /// The fake's shared monotonic clock, advanced by [`FakeCtrl::delay_ms`]. Shared across
    /// parallel tests -- monotonic advance is all any deadline needs.
    static FAKE_NOW: AtomicU64 = AtomicU64::new(0);

    fn fake_now() -> u64 {
        FAKE_NOW.load(Ordering::Relaxed)
    }

    /// One host->module message the fake decoded at its transmit doorbell.
    struct Msg {
        group: u8,
        opcode: u8,
        /// The message body after the 8-byte header: ctrl at 0, any bulk payload at its offset.
        body: Vec<u8>,
    }

    /// One staged module->host event, presented through the CTRL_0 handshake in FIFO order.
    struct Event {
        addr: u32,
        taken: bool,
    }

    /// A socket-event-capable fake module: the send handshake grants a buffer instantly and
    /// decodes each transmitted message at the doorbell; staged events surface one at a time
    /// through the receive handshake (interrupt -> clear -> receive-done pops).
    struct FakeState {
        memory: BTreeMap<u32, u8>,
        sent: Vec<Msg>,
        events: VecDeque<Event>,
        /// Every block write's length -- the 4-byte DMA floor assertion reads this.
        writes: Vec<usize>,
        /// When set, a CMD_DNS_RESOLVE request is auto-answered with this address.
        dns_answer: Option<[u8; 4]>,
        next_event_addr: u32,
    }

    impl FakeState {
        fn new() -> Self {
            Self {
                memory: BTreeMap::new(),
                sent: Vec::new(),
                events: VecDeque::new(),
                writes: Vec::new(),
                dns_answer: None,
                next_event_addr: 0x50000,
            }
        }

        /// Stages one module event: the 4-byte header where the host reads it, the payload
        /// after the padded 8-byte header offset.
        fn stage(&mut self, group: u8, opcode: u8, payload: &[u8]) {
            let addr = self.next_event_addr;
            self.next_event_addr += 0x1000;
            let length = 8 + payload.len() as u16;
            let header = [group, opcode, length as u8, (length >> 8) as u8];
            for (i, byte) in header.iter().enumerate() {
                self.memory.insert(addr + i as u32, *byte);
            }
            for (i, byte) in payload.iter().enumerate() {
                self.memory.insert(addr + 8 + i as u32, *byte);
            }
            self.events.push_back(Event { addr, taken: false });
        }

        fn stage_ip(&mut self, opcode: u8, payload: &[u8]) {
            self.stage(hif::GROUP_IP, opcode, payload);
        }

        fn doorbell(&mut self, value: u32) {
            let addr = value >> 2;
            let read = |memory: &BTreeMap<u32, u8>, at: u32| *memory.get(&at).unwrap_or(&0);
            let group = read(&self.memory, addr);
            let opcode = read(&self.memory, addr + 1);
            let length =
                u16::from_le_bytes([read(&self.memory, addr + 2), read(&self.memory, addr + 3)]);
            let mut body = Vec::new();
            for i in 8..u32::from(length) {
                body.push(read(&self.memory, addr + i));
            }
            if group == hif::GROUP_IP && opcode == socket::CMD_DNS_RESOLVE {
                if let Some(ip) = self.dns_answer {
                    let mut reply = [0u8; 68];
                    let name = body.len().min(64);
                    reply[..name].copy_from_slice(&body[..name]);
                    reply[64..68].copy_from_slice(&ip);
                    self.stage(hif::GROUP_IP, socket::CMD_DNS_RESOLVE, &reply);
                }
            }
            self.sent.push(Msg { group, opcode, body });
        }
    }

    struct FakeBus(Rc<RefCell<FakeState>>);
    struct FakeCtrl(Rc<RefCell<FakeState>>);

    impl Registers for FakeBus {
        fn read_reg(&mut self, addr: u32) -> Result<u32, SpiError> {
            let state = self.0.borrow();
            Ok(match addr {
                CTRL_2 => 0,
                CTRL_4 => 0x40000,
                CTRL_0 => match state.events.front() {
                    Some(event) if !event.taken => {
                        let length = u16::from_le_bytes([
                            *state.memory.get(&(event.addr + 2)).unwrap_or(&0),
                            *state.memory.get(&(event.addr + 3)).unwrap_or(&0),
                        ]);
                        1 | (u32::from(length) << 2)
                    }
                    _ => 0,
                },
                CTRL_1 => state.events.front().map_or(0, |event| event.addr),
                _ => 0,
            })
        }

        fn write_reg(&mut self, addr: u32, value: u32) -> Result<(), SpiError> {
            let mut state = self.0.borrow_mut();
            match addr {
                CTRL_0 => {
                    if value & 1 == 0 {
                        if let Some(event) = state.events.front_mut() {
                            event.taken = true;
                        }
                    }
                    if value & 2 != 0 && state.events.front().is_some_and(|event| event.taken) {
                        state.events.pop_front();
                    }
                }
                CTRL_3 => state.doorbell(value),
                _ => {}
            }
            Ok(())
        }
    }

    impl ModuleBus for FakeBus {
        fn read_block(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), SpiError> {
            let state = self.0.borrow();
            for (offset, slot) in buf.iter_mut().enumerate() {
                *slot = *state.memory.get(&(addr + offset as u32)).unwrap_or(&0);
            }
            Ok(())
        }

        fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<(), SpiError> {
            let mut state = self.0.borrow_mut();
            state.writes.push(data.len());
            for (offset, byte) in data.iter().enumerate() {
                state.memory.insert(addr + offset as u32, *byte);
            }
            Ok(())
        }
    }

    impl WincControl for FakeCtrl {
        fn set_chip_enable(&mut self, _enabled: bool) {}
        fn set_reset(&mut self, _asserted: bool) {}
        fn irq_asserted(&mut self) -> bool {
            let state = self.0.borrow();
            state.events.front().is_some_and(|event| !event.taken)
        }
        fn delay_ms(&mut self, ms: u32) {
            FAKE_NOW.fetch_add(u64::from(ms), Ordering::Relaxed);
        }
    }

    fn net_with(grace: u64) -> (WincNet<FakeBus, FakeCtrl>, Rc<RefCell<FakeState>>) {
        let state = Rc::new(RefCell::new(FakeState::new()));
        let config = WincNetConfig { would_block_grace_ms: grace, ..WincNetConfig::default() };
        let net = WincNet::new(FakeBus(state.clone()), FakeCtrl(state.clone()), fake_now, config);
        (net, state)
    }

    fn ready<T>(result: NetResult<T>) -> T {
        match result {
            NetResult::Ready(value) => value,
            NetResult::WouldBlock => panic!("expected Ready, got WouldBlock"),
            NetResult::Error => panic!("expected Ready, got Error"),
        }
    }

    fn assert_would_block<T>(result: NetResult<T>) {
        assert!(matches!(result, NetResult::WouldBlock), "expected WouldBlock");
    }

    fn assert_error<T>(result: NetResult<T>) {
        assert!(matches!(result, NetResult::Error), "expected Error");
    }

    /// Encodes a `tstrRecvReply` head + its payload at `data_offset` into one event payload.
    fn recv_event(
        remote: SockAddr,
        status: i16,
        data_offset: u16,
        sock: i8,
        session: u16,
        data: &[u8],
    ) -> Vec<u8> {
        let mut payload = vec![0u8; (data_offset as usize + data.len()).max(16)];
        payload[..2].copy_from_slice(&socket::AF_INET.to_le_bytes());
        payload[2..4].copy_from_slice(&remote.port.to_be_bytes());
        payload[4..8].copy_from_slice(&remote.ip);
        payload[8..10].copy_from_slice(&status.to_le_bytes());
        payload[10..12].copy_from_slice(&data_offset.to_le_bytes());
        payload[12] = sock as u8;
        payload[14..16].copy_from_slice(&session.to_le_bytes());
        payload[data_offset as usize..data_offset as usize + data.len()].copy_from_slice(data);
        payload
    }

    fn send_reply(sock: i8, sent: i16, session: u16) -> [u8; 8] {
        let mut raw = [0u8; 8];
        raw[0] = sock as u8;
        raw[2..4].copy_from_slice(&sent.to_le_bytes());
        raw[4..6].copy_from_slice(&session.to_le_bytes());
        raw
    }

    /// Establishes a connected TCP socket on fake sock 0 (send offset 80 from the reply).
    fn connected(
        net: &mut WincNet<FakeBus, FakeCtrl>,
        state: &Rc<RefCell<FakeState>>,
    ) -> SocketHandle {
        let handle = ready(net.tcp_connect(&[142, 251, 157, 119], 443));
        state.borrow_mut().stage_ip(socket::CMD_CONNECT, &[0, 0, 88, 0]);
        ready(net.connect_check(handle));
        handle
    }

    #[test]
    fn connect_posts_the_command_and_resolves_on_the_reply() {
        let (mut net, state) = net_with(0);
        let handle = ready(net.tcp_connect(&[142, 251, 157, 119], 443));
        {
            let s = state.borrow();
            assert_eq!(s.sent.len(), 1);
            assert_eq!(s.sent[0].group, hif::GROUP_IP);
            assert_eq!(s.sent[0].opcode, socket::CMD_CONNECT);
            let expected =
                socket::connect_cmd(0, &SockAddr { port: 443, ip: [142, 251, 157, 119] }, 0, 1);
            assert_eq!(&s.sent[0].body[..12], &expected);
        }
        assert_would_block(net.connect_check(handle));
        state.borrow_mut().stage_ip(socket::CMD_CONNECT, &[0, 0, 88, 0]);
        ready(net.connect_check(handle));
    }

    #[test]
    fn connect_accepts_the_ssl_opcode_sibling() {
        let (mut net, state) = net_with(0);
        let handle = ready(net.tcp_connect(&[1, 2, 3, 4], 443));
        state.borrow_mut().stage_ip(socket::CMD_SSL_CONNECT, &[0, 0, 88, 0]);
        ready(net.connect_check(handle));
    }

    #[test]
    fn connect_failure_carries_through_as_error() {
        let (mut net, state) = net_with(0);
        let handle = ready(net.tcp_connect(&[1, 2, 3, 4], 80));
        state.borrow_mut().stage_ip(socket::CMD_CONNECT, &[0, 0xf4, 1, 40]);
        assert_error(net.connect_check(handle));
    }

    #[test]
    fn send_is_fire_and_forget_with_one_in_flight() {
        let (mut net, state) = net_with(0);
        let handle = connected(&mut net, &state);
        let n = ready(net.send(handle, b"GET / HTTP/1.0\r\n\r\n"));
        assert_eq!(n, 18);
        {
            let s = state.borrow();
            let msg = s.sent.last().unwrap();
            assert_eq!(msg.opcode, socket::CMD_SEND);
            let expected_cmd = socket::send_cmd(0, 18, None, 1);
            assert_eq!(&msg.body[..16], &expected_cmd);
            assert_eq!(&msg.body[80..98], b"GET / HTTP/1.0\r\n\r\n");
        }
        assert_would_block(net.send(handle, b"more"));
        state.borrow_mut().stage_ip(socket::CMD_SEND, &send_reply(0, 18, 1));
        assert_eq!(ready(net.send(handle, b"more")), 4);
    }

    #[test]
    fn a_rejecting_send_reply_marks_the_socket_errored() {
        let (mut net, state) = net_with(0);
        let handle = connected(&mut net, &state);
        assert_eq!(ready(net.send(handle, b"data")), 4);
        state.borrow_mut().stage_ip(socket::CMD_SSL_SEND, &send_reply(0, -9, 1));
        assert_error(net.send(handle, b"data"));
    }

    #[test]
    fn tiny_sends_respect_the_dma_write_floor() {
        let (mut net, state) = net_with(0);
        let handle = connected(&mut net, &state);
        assert_eq!(ready(net.send(handle, b"!")), 1);
        {
            let s = state.borrow();
            let msg = s.sent.last().unwrap();
            assert_eq!(msg.body[80], b'!');
            assert!(
                s.writes.iter().all(|&n| n >= 4),
                "a block write under 4 bytes reached the module"
            );
        }
    }

    #[test]
    fn recv_posts_once_buffers_and_drains() {
        let (mut net, state) = net_with(0);
        let handle = connected(&mut net, &state);
        let mut buf = [0u8; 3];
        assert_would_block(net.recv(handle, &mut buf));
        let posted = state.borrow().sent.len();
        {
            let s = state.borrow();
            let msg = s.sent.last().unwrap();
            assert_eq!(msg.opcode, socket::CMD_RECV);
            let expected = socket::recv_cmd(0, RECV_TIMEOUT_MS, 1, 1024);
            assert_eq!(&msg.body[..12], &expected);
        }
        assert_would_block(net.recv(handle, &mut buf));
        assert_eq!(state.borrow().sent.len(), posted);
        let payload =
            recv_event(SockAddr { port: 443, ip: [142, 251, 157, 119] }, 5, 100, 0, 1, b"hello");
        state.borrow_mut().stage_ip(socket::CMD_RECV, &payload);
        assert_eq!(ready(net.recv(handle, &mut buf)), 3);
        assert_eq!(&buf, b"hel");
        assert_eq!(ready(net.recv(handle, &mut buf)), 2);
        assert_eq!(&buf[..2], b"lo");
        assert_would_block(net.recv(handle, &mut buf));
        assert_eq!(state.borrow().sent.last().unwrap().opcode, socket::CMD_RECV);
        assert_eq!(state.borrow().sent.len(), posted + 1);
    }

    #[test]
    fn recv_reports_fin_as_zero_and_abort_as_error() {
        let (mut net, state) = net_with(0);
        let fin = connected(&mut net, &state);
        let mut buf = [0u8; 8];
        assert_would_block(net.recv(fin, &mut buf));
        let payload = recv_event(SockAddr { port: 443, ip: [1, 2, 3, 4] }, 0, 16, 0, 1, &[]);
        state.borrow_mut().stage_ip(socket::CMD_RECV, &payload);
        assert_eq!(ready(net.recv(fin, &mut buf)), 0);
        net.close(fin);

        let rst = connected(&mut net, &state);
        assert_would_block(net.recv(rst, &mut buf));
        let payload = recv_event(SockAddr { port: 443, ip: [1, 2, 3, 4] }, -12, 16, 0, 2, &[]);
        state.borrow_mut().stage_ip(socket::CMD_SSL_RECV, &payload);
        assert_error(net.recv(rst, &mut buf));
    }

    #[test]
    fn a_stale_session_recv_reply_is_ignored() {
        let (mut net, state) = net_with(0);
        let handle = connected(&mut net, &state);
        let mut buf = [0u8; 8];
        assert_would_block(net.recv(handle, &mut buf));
        let payload = recv_event(SockAddr { port: 443, ip: [1, 2, 3, 4] }, 4, 16, 0, 99, b"stal");
        state.borrow_mut().stage_ip(socket::CMD_RECV, &payload);
        assert_would_block(net.recv(handle, &mut buf));
    }

    #[test]
    fn generations_keep_a_stale_handle_from_aliasing_the_reused_socket() {
        let (mut net, state) = net_with(0);
        let first = connected(&mut net, &state);
        net.close(first);
        assert_eq!(state.borrow().sent.last().unwrap().opcode, socket::CMD_CLOSE);
        let second = ready(net.tcp_connect(&[1, 2, 3, 4], 80));
        assert_ne!(first, second, "the reused socket number must mint a fresh handle");
        assert_error(net.connect_check(first));
        net.register(first, Interest::Read);
        assert_eq!(net.poll(Some(0)), alloc::vec![first]);
        let _ = second;
    }

    #[test]
    fn resolve_answers_literals_locally_and_names_from_the_module() {
        let (mut net, state) = net_with(0);
        assert_eq!(net.resolve("192.168.1.7"), alloc::vec![alloc::vec![192, 168, 1, 7]]);
        assert!(state.borrow().sent.is_empty(), "a literal must not reach the wire");

        state.borrow_mut().dns_answer = Some([142, 250, 74, 36]);
        assert_eq!(net.resolve("example.com"), alloc::vec![alloc::vec![142, 250, 74, 36]]);
        {
            let s = state.borrow();
            assert_eq!(s.sent[0].opcode, socket::CMD_DNS_RESOLVE);
            assert_eq!(&s.sent[0].body, b"example.com\0");
        }
    }

    #[test]
    fn resolve_failure_and_timeout_report_empty() {
        let (mut net, state) = net_with(0);
        state.borrow_mut().dns_answer = Some([0, 0, 0, 0]);
        assert!(net.resolve("nosuch.example").is_empty());
        state.borrow_mut().dns_answer = None;
        assert!(net.resolve("silent.example").is_empty());
    }

    #[test]
    fn listen_chains_bind_then_listen_and_queues_accepts() {
        let (mut net, state) = net_with(0);
        let listener = ready(net.tcp_listen(&[0, 0, 0, 0], 8080, 4));
        assert_eq!(state.borrow().sent[0].opcode, socket::CMD_BIND);
        assert_would_block(net.accept(listener));

        state.borrow_mut().stage_ip(socket::CMD_BIND, &[0, 0, 1, 0]);
        assert_would_block(net.accept(listener));
        {
            let s = state.borrow();
            let listen = s.sent.last().unwrap();
            assert_eq!(listen.opcode, socket::CMD_LISTEN);
            assert_eq!(&listen.body[..4], &socket::listen_cmd(0, 4, 1));
        }
        state.borrow_mut().stage_ip(socket::CMD_LISTEN, &[0, 0, 1, 0]);

        let mut accept_payload = [0u8; 12];
        accept_payload[..8]
            .copy_from_slice(&SockAddr { port: 50000, ip: [192, 168, 1, 9] }.encode());
        accept_payload[8] = 0;
        accept_payload[9] = 3;
        accept_payload[10..12].copy_from_slice(&96u16.to_le_bytes());
        state.borrow_mut().stage_ip(socket::CMD_ACCEPT, &accept_payload);

        let conn = ready(net.accept(listener));
        assert_would_block(net.accept(listener));
        assert_eq!(ready(net.send(conn, b"welcome!")), 8);
        {
            let s = state.borrow();
            let msg = s.sent.last().unwrap();
            assert_eq!(msg.body[0], 3);
            assert_eq!(&msg.body[88..96], b"welcome!");
        }
        assert_eq!(net.local_port(listener), Some(8080));
    }

    #[test]
    fn a_refused_bind_errors_the_listener() {
        let (mut net, state) = net_with(0);
        let listener = ready(net.tcp_listen(&[0, 0, 0, 0], 80, 1));
        state.borrow_mut().stage_ip(socket::CMD_BIND, &[0, 0xf4, 1, 0]);
        assert_error(net.accept(listener));
    }

    #[test]
    fn udp_binds_sends_and_receives_datagrams() {
        let (mut net, state) = net_with(0);
        let udp = ready(net.udp_bind(&[0, 0, 0, 0], 0));
        assert_eq!(net.local_port(udp), Some(49152));
        {
            let s = state.borrow();
            assert_eq!(s.sent[0].opcode, socket::CMD_BIND);
            assert_eq!(s.sent[0].body[8], 7);
            assert_eq!([s.sent[0].body[2], s.sent[0].body[3]], 49152u16.to_be_bytes());
        }
        state.borrow_mut().stage_ip(socket::CMD_BIND, &[7, 0, 1, 0]);

        assert_eq!(ready(net.udp_send_to(udp, b"ping", &[10, 0, 0, 1], 9999)), 4);
        {
            let s = state.borrow();
            let msg = s.sent.last().unwrap();
            assert_eq!(msg.opcode, socket::CMD_SENDTO);
            let expected =
                socket::send_cmd(7, 4, Some(&SockAddr { port: 9999, ip: [10, 0, 0, 1] }), 1);
            assert_eq!(&msg.body[..16], &expected);
            assert_eq!(&msg.body[68..72], b"ping");
        }
        state.borrow_mut().stage_ip(socket::CMD_SENDTO, &send_reply(7, 4, 1));

        let mut buf = [0u8; 16];
        let mut sender = [0u8; 16];
        assert_would_block(net.udp_recv_from(udp, &mut buf, &mut sender));
        assert_eq!(state.borrow().sent.last().unwrap().opcode, socket::CMD_RECVFROM);
        let payload = recv_event(SockAddr { port: 7777, ip: [10, 0, 0, 2] }, 3, 40, 7, 1, b"pon");
        state.borrow_mut().stage_ip(socket::CMD_RECVFROM, &payload);
        let (n, addr_len, port) = ready(net.udp_recv_from(udp, &mut buf, &mut sender));
        assert_eq!((n, addr_len, port), (3, 4, 7777));
        assert_eq!(&buf[..3], b"pon");
        assert_eq!(&sender[..4], &[10, 0, 0, 2]);
    }

    #[test]
    fn poll_reports_registered_readiness_only() {
        let (mut net, state) = net_with(0);
        let handle = connected(&mut net, &state);
        let mut buf = [0u8; 8];
        assert_would_block(net.recv(handle, &mut buf));
        net.register(handle, Interest::Read);
        assert!(net.poll(Some(0)).is_empty());

        let payload = recv_event(SockAddr { port: 443, ip: [1, 2, 3, 4] }, 2, 16, 0, 1, b"ok");
        state.borrow_mut().stage_ip(socket::CMD_RECV, &payload);
        assert_eq!(net.poll(Some(0)), alloc::vec![handle]);

        net.deregister(handle);
        assert!(net.poll(Some(0)).is_empty(), "an unwatched socket never reports");
    }

    #[test]
    fn a_wifi_drop_errors_every_open_socket() {
        let (mut net, state) = net_with(0);
        let handle = ready(net.tcp_connect(&[1, 2, 3, 4], 80));
        state.borrow_mut().stage(
            hif::GROUP_WIFI,
            crate::wifi::RESP_CON_STATE_CHANGED,
            &[0, 1, 0, 0],
        );
        assert_error(net.connect_check(handle));
        net.register(handle, Interest::Write);
        assert_eq!(net.poll(Some(0)), alloc::vec![handle]);
    }

    #[test]
    fn dropping_the_backend_closes_every_open_socket() {
        let (mut net, state) = net_with(0);
        let a = connected(&mut net, &state);
        let b = ready(net.tcp_connect(&[1, 2, 3, 4], 80));
        let _ = (a, b);
        let before = state.borrow().sent.len();
        drop(net);
        let s = state.borrow();
        let closes = s.sent[before..].iter().filter(|msg| msg.opcode == socket::CMD_CLOSE).count();
        assert_eq!(closes, 2);
    }
}
