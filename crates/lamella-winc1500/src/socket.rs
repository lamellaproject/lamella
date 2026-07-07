//! The socket group (HIF group 2) wire layer: command opcodes, request builders, and reply
//! decoders for the WINC's on-module TCP/IP + TLS stack -- the layer the interpreter's
//! `NetBackend` mapping will drive once an association is up.

/// `m2m_socket_host_if.h` command values (HIF group 2 opcodes; 0x53..0x57 are the 19.7-era
/// additions).
pub const CMD_BIND: u8 = 0x41;
pub const CMD_LISTEN: u8 = 0x42;
pub const CMD_ACCEPT: u8 = 0x43;
pub const CMD_CONNECT: u8 = 0x44;
pub const CMD_SEND: u8 = 0x45;
pub const CMD_RECV: u8 = 0x46;
pub const CMD_SENDTO: u8 = 0x47;
pub const CMD_RECVFROM: u8 = 0x48;
pub const CMD_CLOSE: u8 = 0x49;
pub const CMD_DNS_RESOLVE: u8 = 0x4a;
pub const CMD_SSL_CONNECT: u8 = 0x4b;
pub const CMD_SSL_SEND: u8 = 0x4c;
pub const CMD_SSL_RECV: u8 = 0x4d;
pub const CMD_SSL_CLOSE: u8 = 0x4e;
pub const CMD_SET_SOCKET_OPTION: u8 = 0x4f;
pub const CMD_SSL_CREATE: u8 = 0x50;
pub const CMD_SSL_SET_SOCK_OPT: u8 = 0x51;
pub const CMD_PING: u8 = 0x52;
pub const CMD_SSL_SET_CS_LIST: u8 = 0x53;
pub const CMD_SSL_BIND: u8 = 0x54;
pub const CMD_SSL_EXP_CHECK: u8 = 0x55;
pub const CMD_SECURE: u8 = 0x56;
pub const CMD_SSL_CONNECT_ALPN: u8 = 0x57;

/// `socket.h`: request on-module TLS for a TCP socket (create with [`CMD_SSL_CREATE`], connect
/// with [`CMD_SSL_CONNECT`], move data with the SSL send/recv commands).
pub const FLAG_SSL: u8 = 0x01;

/// The per-socket SSL flag byte carried in the connect command's `u8SslFlags` (the reference's
/// `SSL_FLAGS_*`). The 19.7 driver creates every SSL socket with `ACTIVE | NO_TX_COPY` and ORs
/// in the option-driven bits; a successful connect reply's opcode does NOT depend on them.
pub const SSL_ACTIVE: u8 = 0x01;
pub const SSL_BYPASS_X509: u8 = 0x02;
pub const SSL_CACHE_SESSION: u8 = 0x10;
pub const SSL_NO_TX_COPY: u8 = 0x20;
pub const SSL_CHECK_SNI: u8 = 0x40;
pub const SSL_DELAY: u8 = 0x80;

/// `socket.h` SSL socket options for [`ssl_set_sock_opt_cmd`] (level `SOL_SSL_SOCKET`). The
/// flag-valued options (bypass/caching/SNI-validation) never reach the wire in the reference --
/// they fold into the connect command's flag byte -- so only [`SO_SSL_SNI`] and [`SO_SSL_ALPN`]
/// are sent as [`CMD_SSL_SET_SOCK_OPT`] requests.
pub const SO_SSL_BYPASS_X509_VERIF: u8 = 0x01;
pub const SO_SSL_SNI: u8 = 0x02;
pub const SO_SSL_ENABLE_SESSION_CACHING: u8 = 0x03;
pub const SO_SSL_ENABLE_SNI_VALIDATION: u8 = 0x04;
pub const SO_SSL_ALPN: u8 = 0x05;

/// `HOSTNAME_MAX_SIZE`: the SNI value (hostname + NUL) must stay under this; it is also the
/// option-value capacity of the set-sockopt command.
pub const HOSTNAME_MAX: usize = 64;

/// Host-assigned socket-number partitions (`socket.h`).
pub const TCP_SOCKETS: core::ops::Range<i8> = 0..7;
pub const UDP_SOCKETS: core::ops::Range<i8> = 7..11;

/// AF_INET, the only family the module speaks.
pub const AF_INET: u16 = 2;

/// The reference's send-payload offsets within a HIF message body (`socket.c`: Ethernet +
/// IP headers are prepended in place around the payload, so the host leaves room): TCP at 80,
/// UDP at 68, TLS at 85 -- all derived from `IP_PACKET_OFFSET` 40. A CONNECTED socket uses the
/// offset its connect/accept reply carried (equal to these on current firmware); a UDP sendto
/// has no reply to learn from, so it uses the constant.
pub const TCP_TX_OFFSET: u16 = 80;
pub const UDP_TX_OFFSET: u16 = 68;
pub const SSL_TX_OFFSET: u16 = 85;

/// `SOCKET_BUFFER_MAX_LENGTH`: the largest payload one send/sendto command may carry.
pub const SEND_MAX: usize = 1400;

/// `SOCK_ERR_TIMEOUT` (`socket.h`): a posted receive's window elapsed with nothing to
/// deliver -- benign (repost), unlike the connection-state errors around it.
pub const ERR_TIMEOUT: i16 = -13;

/// `tstrSockAddr` (8 bytes): family, then port and address in NETWORK byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SockAddr {
    pub port: u16,
    pub ip: [u8; 4],
}

impl SockAddr {
    pub(crate) fn encode(&self) -> [u8; 8] {
        let mut raw = [0u8; 8];
        raw[..2].copy_from_slice(&AF_INET.to_le_bytes());
        raw[2..4].copy_from_slice(&self.port.to_be_bytes());
        raw[4..8].copy_from_slice(&self.ip);
        raw
    }

    fn decode(raw: &[u8]) -> Self {
        Self { port: u16::from_be_bytes([raw[2], raw[3]]), ip: [raw[4], raw[5], raw[6], raw[7]] }
    }
}

/// `tstrConnectCmd` (12 bytes): the peer address, the host-chosen socket number, the SSL flags,
/// and the session id. Sent under [`CMD_CONNECT`] (or [`CMD_SSL_CONNECT`] for a TLS socket).
pub fn connect_cmd(sock: i8, addr: &SockAddr, ssl_flags: u8, session: u16) -> [u8; 12] {
    let mut raw = [0u8; 12];
    raw[..8].copy_from_slice(&addr.encode());
    raw[8] = sock as u8;
    raw[9] = ssl_flags;
    raw[10..12].copy_from_slice(&session.to_le_bytes());
    raw
}

/// `tstrConnectReply` (4 bytes): the socket, the firmware's error (0 = connected), and a final
/// union field -- the app-data offset for send messages on success, an error-source/error-code
/// pair on failure (19.7 layout). Opcode note: 19.7.3 firmware answers an SSL connect under
/// [`CMD_SSL_CONNECT`] for success AND failure (silicon-verified both ways: error 0 with a real
/// offset, and error -12 when the peer cannot complete TLS), while the 19.7.7-era reference
/// notes later firmware reports successes under [`CMD_CONNECT`] -- a receiver should accept
/// both opcodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectReply {
    pub sock: i8,
    pub error: i8,
    pub app_data_offset: u16,
}

impl ConnectReply {
    pub fn decode(raw: &[u8; 4]) -> Self {
        Self {
            sock: raw[0] as i8,
            error: raw[1] as i8,
            app_data_offset: u16::from_le_bytes([raw[2], raw[3]]),
        }
    }

    /// On a FAILED connect, the union's first byte: 0 = no detail, 1 = a TLS alert received
    /// from the peer, 2 = a TLS alert generated locally.
    pub fn err_source(&self) -> u8 {
        self.app_data_offset.to_le_bytes()[0]
    }

    /// On a failed connect with a TLS-alert source, the union's second byte: the alert id.
    pub fn err_code(&self) -> u8 {
        self.app_data_offset.to_le_bytes()[1]
    }
}

/// `tstrSendCmd` (16 bytes). The payload itself rides as the HIF data buffer at the
/// connect-reply's app-data offset; `addr` is zero for connected (TCP) sends and the
/// destination for [`CMD_SENDTO`].
pub fn send_cmd(sock: i8, data_len: u16, addr: Option<&SockAddr>, session: u16) -> [u8; 16] {
    let mut raw = [0u8; 16];
    raw[0] = sock as u8;
    raw[2..4].copy_from_slice(&data_len.to_le_bytes());
    if let Some(addr) = addr {
        raw[4..12].copy_from_slice(&addr.encode());
    }
    raw[12..14].copy_from_slice(&session.to_le_bytes());
    raw
}

/// `tstrRecvCmd` (12 bytes in the 19.7 layout): the receive-window timeout, the socket, the
/// session, and the host's buffer capacity (`u16BufLen`, new over the 19.4-era 8-byte shape).
pub fn recv_cmd(sock: i8, timeout_ms: u32, session: u16, buf_len: u16) -> [u8; 12] {
    let mut raw = [0u8; 12];
    raw[..4].copy_from_slice(&timeout_ms.to_le_bytes());
    raw[4] = sock as u8;
    raw[6..8].copy_from_slice(&session.to_le_bytes());
    raw[8..10].copy_from_slice(&buf_len.to_le_bytes());
    raw
}

/// `tstrSSLSetSockOptCmd` (72 bytes) under [`CMD_SSL_SET_SOCK_OPT`]: sets a value-carrying SSL
/// option on a created-but-not-yet-connected SSL socket. For [`SO_SSL_SNI`] the value is the
/// server hostname WITH its NUL terminator; the firmware then offers it in the ClientHello.
/// Returns `None` when the value exceeds the command's capacity ([`HOSTNAME_MAX`], and SNI
/// must be strictly shorter to keep the NUL).
pub fn ssl_set_sock_opt_cmd(sock: i8, option: u8, value: &[u8], session: u16) -> Option<[u8; 72]> {
    if value.len() > HOSTNAME_MAX || (option == SO_SSL_SNI && value.len() >= HOSTNAME_MAX) {
        return None;
    }
    let mut raw = [0u8; 72];
    raw[0] = sock as u8;
    raw[1] = option;
    raw[2..4].copy_from_slice(&session.to_le_bytes());
    raw[4..8].copy_from_slice(&(value.len() as u32).to_le_bytes());
    raw[8..8 + value.len()].copy_from_slice(value);
    Some(raw)
}

/// `tstrCloseCmd` (4 bytes).
pub fn close_cmd(sock: i8, session: u16) -> [u8; 4] {
    let mut raw = [0u8; 4];
    raw[0] = sock as u8;
    raw[2..4].copy_from_slice(&session.to_le_bytes());
    raw
}

/// `tstrBindCmd` (12 bytes): the local address to bind, the socket, and the session. Sent under
/// [`CMD_BIND`] for a listener or a UDP socket.
pub fn bind_cmd(sock: i8, addr: &SockAddr, session: u16) -> [u8; 12] {
    let mut raw = [0u8; 12];
    raw[..8].copy_from_slice(&addr.encode());
    raw[8] = sock as u8;
    raw[10..12].copy_from_slice(&session.to_le_bytes());
    raw
}

/// `tstrListenCmd` (4 bytes): the bound socket and its backlog. Sent under [`CMD_LISTEN`]
/// after the bind reply confirms.
pub fn listen_cmd(sock: i8, backlog: u8, session: u16) -> [u8; 4] {
    let mut raw = [0u8; 4];
    raw[0] = sock as u8;
    raw[1] = backlog;
    raw[2..4].copy_from_slice(&session.to_le_bytes());
    raw
}

/// `tstrBindReply` / `tstrListenReply` (4 bytes, one shape): the socket, a status (0 = ok,
/// negative = the firmware refused), and the session echo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusReply {
    pub sock: i8,
    pub status: i8,
    pub session: u16,
}

impl StatusReply {
    pub fn decode(raw: &[u8; 4]) -> Self {
        Self {
            sock: raw[0] as i8,
            status: raw[1] as i8,
            session: u16::from_le_bytes([raw[2], raw[3]]),
        }
    }
}

/// `tstrAcceptReply` (12 bytes), arriving UNSOLICITED on a listening socket: the peer address,
/// the listener it arrived on, the FIRMWARE-assigned connected socket number, and the connected
/// socket's send app-data offset (same meaning as the connect reply's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcceptReply {
    pub remote: SockAddr,
    pub listen_sock: i8,
    pub connected_sock: i8,
    pub app_data_offset: u16,
}

impl AcceptReply {
    pub fn decode(raw: &[u8; 12]) -> Self {
        Self {
            remote: SockAddr::decode(&raw[..8]),
            listen_sock: raw[8] as i8,
            connected_sock: raw[9] as i8,
            app_data_offset: u16::from_le_bytes([raw[10], raw[11]]),
        }
    }
}

/// `tstrSendReply` (8 bytes): the socket, the byte count the firmware accepted (negative = a
/// socket error), and the session echo. Opcode note: an SSL socket's ACCEPTED send answers
/// under plain [`CMD_SEND`] (reply-opcode folding, silicon-verified) -- match send replies by
/// SOCKET, accepting the whole send-opcode trio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendReply {
    pub sock: i8,
    pub sent: i16,
    pub session: u16,
}

impl SendReply {
    pub fn decode(raw: &[u8; 8]) -> Self {
        Self {
            sock: raw[0] as i8,
            sent: i16::from_le_bytes([raw[2], raw[3]]),
            session: u16::from_le_bytes([raw[4], raw[5]]),
        }
    }
}

/// `tstrDnsReply` (68 bytes): the echoed hostname and the resolved IPv4 address -- the four
/// bytes at offset 64, already in network (octet) order; all-zero means resolution failed.
pub fn dns_reply_ip(raw: &[u8; 68]) -> [u8; 4] {
    [raw[64], raw[65], raw[66], raw[67]]
}

/// `tstrRecvReply` (16 bytes): the remote address, the receive STATUS (bytes received, or a
/// negative socket error), and the offset of the received bytes within the reply message's
/// payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvReply {
    pub remote: SockAddr,
    pub status: i16,
    pub data_offset: u16,
    pub sock: i8,
    pub session: u16,
}

impl RecvReply {
    pub fn decode(raw: &[u8; 16]) -> Self {
        Self {
            remote: SockAddr::decode(&raw[..8]),
            status: i16::from_le_bytes([raw[8], raw[9]]),
            data_offset: u16::from_le_bytes([raw[10], raw[11]]),
            sock: raw[12] as i8,
            session: u16::from_le_bytes([raw[14], raw[15]]),
        }
    }
}

/// The host-side socket bookkeeping the reference keeps: number assignment within the TCP/UDP
/// partitions and the rolling session id every command is stamped with.
#[derive(Debug, Default)]
pub struct SocketTable {
    tcp_used: u8,
    udp_used: u8,
    session: u16,
}

impl SocketTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claims a free TCP socket number (0..6), or `None` when all seven are in use.
    pub fn claim_tcp(&mut self) -> Option<i8> {
        let slot = (0..7).find(|i| self.tcp_used & (1 << i) == 0)?;
        self.tcp_used |= 1 << slot;
        Some(slot as i8)
    }

    /// Claims a free UDP socket number (7..10).
    pub fn claim_udp(&mut self) -> Option<i8> {
        let slot = (0..4).find(|i| self.udp_used & (1 << i) == 0)?;
        self.udp_used |= 1 << slot;
        Some(7 + slot as i8)
    }

    /// Claims a SPECIFIC socket number the FIRMWARE assigned (an accept reply's connected
    /// socket): `false` if it is out of range or already claimed host-side (a state mismatch
    /// the caller should treat as a refused connection).
    pub fn claim_specific(&mut self, sock: i8) -> bool {
        if TCP_SOCKETS.contains(&sock) && self.tcp_used & (1 << sock) == 0 {
            self.tcp_used |= 1 << sock;
            return true;
        }
        if UDP_SOCKETS.contains(&sock) && self.udp_used & (1 << (sock - 7)) == 0 {
            self.udp_used |= 1 << (sock - 7);
            return true;
        }
        false
    }

    /// Releases a socket number back to its partition.
    pub fn release(&mut self, sock: i8) {
        if TCP_SOCKETS.contains(&sock) {
            self.tcp_used &= !(1 << sock);
        } else if UDP_SOCKETS.contains(&sock) {
            self.udp_used &= !(1 << (sock - 7));
        }
    }

    /// The next session id (rolling, never zero -- the reference starts at 1).
    pub fn next_session(&mut self) -> u16 {
        self.session = self.session.wrapping_add(1);
        if self.session == 0 {
            self.session = 1;
        }
        self.session
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_cmd_encodes_address_and_flags() {
        let addr = SockAddr { port: 443, ip: [93, 184, 216, 34] };
        let raw = connect_cmd(1, &addr, FLAG_SSL, 7);
        assert_eq!(&raw[..2], &AF_INET.to_le_bytes());
        assert_eq!([raw[2], raw[3]], 443u16.to_be_bytes());
        assert_eq!(&raw[4..8], &[93, 184, 216, 34]);
        assert_eq!(raw[8], 1);
        assert_eq!(raw[9], FLAG_SSL);
        assert_eq!([raw[10], raw[11]], 7u16.to_le_bytes());
    }

    #[test]
    fn replies_decode() {
        let connect = ConnectReply::decode(&[3, 0, 0xe8, 0x00]);
        assert_eq!(connect.sock, 3);
        assert_eq!(connect.error, 0);
        assert_eq!(connect.app_data_offset, 0xe8);

        let mut raw = [0u8; 16];
        raw[..8].copy_from_slice(&SockAddr { port: 80, ip: [10, 0, 0, 1] }.encode());
        raw[8..10].copy_from_slice(&512i16.to_le_bytes());
        raw[10..12].copy_from_slice(&100u16.to_le_bytes());
        raw[12] = 2;
        let recv = RecvReply::decode(&raw);
        assert_eq!(recv.remote.port, 80);
        assert_eq!(recv.status, 512);
        assert_eq!(recv.data_offset, 100);
        assert_eq!(recv.sock, 2);
    }

    #[test]
    fn recv_cmd_carries_timeout_session_and_buffer_capacity() {
        let raw = recv_cmd(2, 30_000, 5, 800);
        assert_eq!(&raw[..4], &30_000u32.to_le_bytes());
        assert_eq!(raw[4], 2);
        assert_eq!([raw[6], raw[7]], 5u16.to_le_bytes());
        assert_eq!([raw[8], raw[9]], 800u16.to_le_bytes());
        assert_eq!([raw[10], raw[11]], [0, 0]);
    }

    #[test]
    fn ssl_sockopt_encodes_a_nul_terminated_sni() {
        let raw = ssl_set_sock_opt_cmd(0, SO_SSL_SNI, b"www.google.com\0", 1).expect("fits");
        assert_eq!(raw.len(), 72);
        assert_eq!(raw[0], 0);
        assert_eq!(raw[1], SO_SSL_SNI);
        assert_eq!([raw[2], raw[3]], 1u16.to_le_bytes());
        assert_eq!(&raw[4..8], &15u32.to_le_bytes());
        assert_eq!(&raw[8..23], b"www.google.com\0");
        assert!(raw[23..].iter().all(|&b| b == 0));
    }

    #[test]
    fn ssl_sockopt_rejects_an_oversized_value() {
        assert_eq!(ssl_set_sock_opt_cmd(0, SO_SSL_SNI, &[b'a'; 64], 1), None);
        assert_eq!(ssl_set_sock_opt_cmd(0, SO_SSL_ALPN, &[0u8; 65], 1), None);
        assert!(ssl_set_sock_opt_cmd(0, SO_SSL_ALPN, &[0u8; 64], 1).is_some());
    }

    #[test]
    fn failed_connect_reply_exposes_the_tls_alert() {
        let reply = ConnectReply::decode(&[0, 0xf3, 1, 40]);
        assert_eq!(reply.error, -13);
        assert_eq!(reply.err_source(), 1);
        assert_eq!(reply.err_code(), 40);
    }

    #[test]
    fn socket_table_partitions_and_sessions() {
        let mut table = SocketTable::new();
        assert_eq!(table.claim_tcp(), Some(0));
        assert_eq!(table.claim_tcp(), Some(1));
        assert_eq!(table.claim_udp(), Some(7));
        table.release(0);
        assert_eq!(table.claim_tcp(), Some(0));
        for _ in 0..5 {
            table.claim_tcp();
        }
        assert_eq!(table.claim_tcp(), None);
        assert_eq!(table.next_session(), 1);
        assert_eq!(table.next_session(), 2);
    }

    #[test]
    fn claim_specific_takes_a_firmware_assigned_number() {
        let mut table = SocketTable::new();
        assert!(table.claim_specific(3));
        assert!(!table.claim_specific(3));
        assert!(!table.claim_specific(11));
        assert!(!table.claim_specific(-1));
        assert_eq!(table.claim_tcp(), Some(0));
        assert_eq!(table.claim_tcp(), Some(1));
        assert_eq!(table.claim_tcp(), Some(2));
        assert_eq!(table.claim_tcp(), Some(4));
        table.release(3);
        assert!(table.claim_specific(3));
    }

    #[test]
    fn bind_and_listen_encode() {
        let addr = SockAddr { port: 8080, ip: [0, 0, 0, 0] };
        let bind = bind_cmd(2, &addr, 9);
        assert_eq!(&bind[..2], &AF_INET.to_le_bytes());
        assert_eq!([bind[2], bind[3]], 8080u16.to_be_bytes());
        assert_eq!(bind[8], 2);
        assert_eq!([bind[10], bind[11]], 9u16.to_le_bytes());

        let listen = listen_cmd(2, 4, 9);
        assert_eq!(listen, [2, 4, 9, 0]);
    }

    #[test]
    fn status_accept_send_and_dns_replies_decode() {
        let status = StatusReply::decode(&[2, 0xf4, 9, 0]);
        assert_eq!(status.sock, 2);
        assert_eq!(status.status, -12);
        assert_eq!(status.session, 9);

        let mut raw = [0u8; 12];
        raw[..8].copy_from_slice(&SockAddr { port: 50000, ip: [192, 168, 1, 7] }.encode());
        raw[8] = 1;
        raw[9] = 4;
        raw[10..12].copy_from_slice(&0x65u16.to_le_bytes());
        let accept = AcceptReply::decode(&raw);
        assert_eq!(accept.listen_sock, 1);
        assert_eq!(accept.connected_sock, 4);
        assert_eq!(accept.app_data_offset, 0x65);
        assert_eq!(accept.remote.port, 50000);

        let send = SendReply::decode(&[0, 0, 0xf7, 0xff, 1, 0, 0, 0]);
        assert_eq!(send.sock, 0);
        assert_eq!(send.sent, -9);
        assert_eq!(send.session, 1);

        let mut dns = [0u8; 68];
        dns[64..68].copy_from_slice(&[142, 251, 157, 119]);
        assert_eq!(dns_reply_ip(&dns), [142, 251, 157, 119]);
    }
}
