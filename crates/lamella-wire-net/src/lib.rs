//! Lamella Link over TCP, for the side of the link that has no `std`.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use lamella_net_core::{Interest, NetBackend, NetResult, SocketHandle};
use lamella_wire::{Frame, FrameReader, Transport, TransportError, encode_frame};

/// Bytes read from the socket per `recv` call. Matches the other carriers' read size; a frame
/// larger than this simply takes several reads, which the frame reader is built to absorb.
const READ_CHUNK: usize = 512;

/// How many `recv` calls one poll makes before returning, so a peer sending without pause cannot
/// keep the caller inside `poll` forever. A serve loop calls `poll` repeatedly, so a budget costs
/// only the loop iteration it defers to; without one, a flooding peer starves everything else the
/// caller does between polls.
const READS_PER_POLL: usize = 8;

/// The default cap on bytes queued for transmission but not yet accepted by the socket, past
/// which a further frame is refused rather than queued. See [`TcpLink::with_send_cap`].
///
/// Sized at two of the largest frames this protocol can carry. The debug and deploy channels are
/// request-and-response, so a link that is keeping up holds nothing here at all; a backlog beyond
/// this means the far side has stopped reading, which is a condition to report rather than one to
/// absorb.
pub const DEFAULT_SEND_CAP: usize = 2 * (lamella_wire::MAX_PAYLOAD + 16);

/// A Lamella Link session over one connected TCP socket: the frame reader, the queue of bytes the
/// socket has not accepted yet, and the socket handle itself.
///
/// The backend is NOT owned here. Every operation takes it as an argument, so this composes with a
/// device that also runs managed code against the same network stack -- there is one backend on
/// the board and this holds no claim on it. [`TcpTransport`] is the wrapper for the common case
/// where the wire is the only thing using the network, and it is what implements [`Transport`].
///
/// # This does not reconnect, and that is the design
///
/// A dropped connection is reported as [`TransportError::Closed`] and the link stays closed. It
/// would be a few lines to dial again underneath, and it would be wrong: a new connection is a new
/// SESSION. The version handshake that opened the old one does not carry over, the capabilities
/// the host negotiated were the old peer's, and a target that dropped the connection because it
/// reset is not in the state the caller believes it is in.
///
/// Reconnecting behind the seam would make all of that invisible at exactly the moment it decides
/// whether the next operation is correct. A caller that wants to reconnect can, and when it does
/// it re-opens a session deliberately, with a fresh handshake, which is the only way the result is
/// something it can act on.
pub struct TcpLink {
    socket: SocketHandle,
    reader: FrameReader,
    /// Bytes encoded and committed but not yet accepted by the socket, oldest first. Strictly
    /// FIFO: nothing is written to the socket while anything is ahead of it here.
    outbound: Vec<u8>,
    send_cap: usize,
    /// Why this session ended, once it has. Frames still in hand are delivered after this is set,
    /// and so are frames still arriving -- see [`TcpLink::poll_frame`]. First ending wins: a send
    /// failure is more actionable than the clean close that may follow it.
    ended: Option<TransportError>,
    /// Whether the RECEIVE side has reached its end. Tracked apart from `ended` because the two
    /// directions of a socket fail independently: a peer that half-closes after replying breaks
    /// the next send while leaving its reply perfectly readable, and one flag cannot hold both
    /// facts without the send failure suppressing the read.
    read_ended: bool,
}

impl TcpLink {
    /// A session over an already-connected `socket`, with the default send cap.
    ///
    /// How the socket was obtained is deliberately not this type's business: a listener's
    /// [`accept_link`] and an outbound [`NetBackend::tcp_connect`] produce the same thing, and a
    /// board that must dial out is then no different from one that is dialed into.
    #[must_use]
    pub fn new(socket: SocketHandle) -> Self {
        Self::with_send_cap(socket, DEFAULT_SEND_CAP)
    }

    /// A session with an explicit cap on un-accepted outbound bytes ([`DEFAULT_SEND_CAP`]).
    ///
    /// Worth setting on a part whose whole heap is smaller than the default: the queue is the one
    /// unbounded thing a slow reader on the far side can grow, and on a device the alternative to
    /// a cap is running out of memory in a code path that has no way to report it.
    ///
    /// It must admit the LARGEST frame the caller will ever send, header and checksum included.
    /// A frame bigger than the cap is refused whatever the queue is doing, because the refusal
    /// compares the whole frame against the whole cap -- there is no state in which it fits. A
    /// caller that has sized this down should chunk to match, the way the deploy path already
    /// does for the wire's own length limit.
    #[must_use]
    pub fn with_send_cap(socket: SocketHandle, send_cap: usize) -> Self {
        Self {
            socket,
            reader: FrameReader::new(),
            outbound: Vec::new(),
            send_cap,
            ended: None,
            read_ended: false,
        }
    }

    /// Record why the session ended, keeping the FIRST reason. A send failure followed by the
    /// peer's clean close must report the failure: it is the one with a remedy attached.
    fn end(&mut self, why: TransportError) {
        if self.ended.is_none() {
            self.ended = Some(why);
        }
    }

    /// The socket this session runs on -- what a caller registers for readiness, or closes.
    #[must_use]
    pub fn socket(&self) -> SocketHandle {
        self.socket
    }

    /// Bytes committed to the wire that the socket has not accepted yet. `0` means everything
    /// written has been handed to the stack.
    ///
    /// A caller that is about to stop polling (to sleep, or to run an uninterruptible burst) can
    /// read this to know whether anything would be left in hand.
    #[must_use]
    pub fn pending_out(&self) -> usize {
        self.outbound.len()
    }

    /// Whether this session has ended (the peer closed, or the carrier failed). A closed link
    /// still yields any frames that arrived before it closed.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.ended.is_some()
    }

    /// Push as much of the outbound queue into the socket as it will accept. `Ok(true)` when the
    /// queue is empty afterward.
    ///
    /// Called for you by [`TcpLink::send_frame`] and [`TcpLink::poll_frame`]; public because a
    /// caller that is neither sending nor polling -- one about to idle, say -- must still give
    /// the queue a chance to drain.
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if the socket failed, or the session's ending if it already
    /// had one. An ended session reports that rather than `Ok(true)`: its queue is empty because
    /// it was discarded, and "everything drained" is not what happened to it.
    pub fn flush<N: NetBackend + ?Sized>(&mut self, net: &mut N) -> Result<bool, TransportError> {
        if let Some(ended) = self.ended {
            return Err(ended);
        }
        while !self.outbound.is_empty() {
            match net.send(self.socket, &self.outbound) {
                NetResult::Ready(0) | NetResult::WouldBlock => break,
                NetResult::Ready(taken) => {
                    let taken = taken.min(self.outbound.len());
                    self.outbound.drain(..taken);
                }
                NetResult::Error => {
                    self.end(TransportError::Carrier);
                    self.outbound.clear();
                    return Err(TransportError::Carrier);
                }
            }
        }
        Ok(self.outbound.is_empty())
    }

    /// Encode one frame and commit it to the wire, sending as much as the socket takes now and
    /// queuing the rest.
    ///
    /// Returning `Ok(())` means the frame is committed and will be sent in order, not that its
    /// bytes have left the machine -- the same promise the other carriers make, where the operating
    /// system or the USB stack holds the tail.
    ///
    /// # Errors
    /// - [`TransportError::PayloadTooLarge`] if the payload exceeds what a frame's length field can
    ///   count. Nothing is queued, so the stream is untouched and the caller may chunk and retry.
    /// - [`TransportError::Carrier`] if the socket failed, or if the un-accepted queue is already
    ///   at its cap. In the second case the frame is refused WHOLE and the queue is left intact:
    ///   partially queuing it would put a fragment in a stream the far side reassembles by length.
    /// - The session's own ending if it has already ended -- [`TransportError::Closed`] after a
    ///   clean close, [`TransportError::Carrier`] after a failure. The first reason is kept, so
    ///   what a caller reads here is why the session stopped rather than what it hit last.
    pub fn send_frame<N: NetBackend + ?Sized>(
        &mut self,
        net: &mut N,
        msg_type: u8,
        seq: u16,
        payload: &[u8],
    ) -> Result<(), TransportError> {
        if let Some(ended) = self.ended {
            return Err(ended);
        }
        let frame = encode_frame(msg_type, seq, payload).ok_or(TransportError::PayloadTooLarge)?;
        if self.outbound.len() + frame.len() > self.send_cap {
            self.flush(net)?;
            if self.outbound.len() + frame.len() > self.send_cap {
                return Err(TransportError::Carrier);
            }
        }
        self.outbound.extend_from_slice(&frame);
        self.flush(net)?;
        Ok(())
    }

    /// The next frame received, or `None` if none is complete yet. Also drains the outbound queue,
    /// so a caller that only polls still makes progress on what it has sent.
    ///
    /// # Frames outlive the connection that carried them
    ///
    /// A peer that answers and then closes is ordinary -- a target replying to the last request of
    /// a session does exactly that, and the close can arrive in the same read as the reply. So a
    /// clean close is remembered rather than reported at once, and the frames already in hand are
    /// delivered first; [`TransportError::Closed`] follows when there are none left. Reporting the
    /// close immediately would discard an answer that had already arrived, and the caller would
    /// see a session that ended without replying.
    ///
    /// **The same holds when the SEND side is what failed**, which is the half that is easy to
    /// miss. A peer that replies and then resets leaves an answer sitting in the reader and a
    /// broken socket underneath it, and the drain this does on the way past is where the break
    /// surfaces. That drain ends the session and does not report it: the answer is delivered
    /// first, exactly as for a clean close. **A failure to send is not a reason to discard
    /// something already received.**
    ///
    /// # Errors
    /// [`TransportError::Closed`] once the peer's clean close is reached with no frames left, or
    /// [`TransportError::Carrier`] if the socket failed and there is nothing left to deliver.
    pub fn poll_frame<N: NetBackend + ?Sized>(
        &mut self,
        net: &mut N,
    ) -> Result<Option<Frame>, TransportError> {
        if self.ended.is_none() {
            let _ = self.flush(net);
        }
        if let Some(frame) = self.reader.next_frame() {
            return Ok(Some(frame));
        }
        if !self.read_ended {
            let mut buf = [0u8; READ_CHUNK];
            for _ in 0..READS_PER_POLL {
                match net.recv(self.socket, &mut buf) {
                    NetResult::Ready(0) => {
                        self.read_ended = true;
                        self.end(TransportError::Closed);
                        break;
                    }
                    NetResult::Ready(count) => self.reader.push(&buf[..count.min(buf.len())]),
                    NetResult::WouldBlock => break,
                    NetResult::Error => {
                        self.read_ended = true;
                        self.end(TransportError::Carrier);
                        break;
                    }
                }
            }
        }
        match (self.reader.next_frame(), self.ended) {
            (Some(frame), _) => Ok(Some(frame)),
            (None, Some(ended)) => Err(ended),
            (None, None) => Ok(None),
        }
    }

    /// Ask the backend to watch this socket for arriving data, so the caller can park until there
    /// is something to poll for instead of spinning on [`TcpLink::poll_frame`].
    ///
    /// Optional. A serve loop that polls the wire between interpreter bursts needs nothing here;
    /// a board that wants to idle between requests does, and on a battery-powered one the
    /// difference is the whole of its power budget.
    pub fn register_read<N: NetBackend + ?Sized>(&mut self, net: &mut N) {
        net.register(self.socket, Interest::Read);
    }

    /// Drop this socket from the backend's readiness set, undoing [`TcpLink::register_read`].
    pub fn deregister<N: NetBackend + ?Sized>(&mut self, net: &mut N) {
        net.deregister(self.socket);
    }

    /// Close the socket and end the session, discarding anything the socket never accepted.
    ///
    /// A caller with queued bytes that matter should [`TcpLink::flush`] to `Ok(true)` first. This
    /// does not, because a close that waits is a close that can block, and the case this is
    /// reached from most often is one where the far side is already gone.
    pub fn close<N: NetBackend + ?Sized>(&mut self, net: &mut N) {
        net.close(self.socket);
        self.outbound.clear();
        self.read_ended = true;
        self.end(TransportError::Closed);
    }
}

/// Open a listening socket for Lamella Link on `addr:port` -- the board-serves-the-link direction.
///
/// `addr` is the bind address in network byte order, 4 bytes for IPv4 or 16 for IPv6; all zeroes
/// binds every interface. A `port` of `0` binds an ephemeral one, readable back through
/// [`NetBackend::local_port`].
///
/// The backlog is 1. A running program has one virtual machine to debug and one image to deploy
/// into, so a second concurrent session is not a thing this protocol has an answer for: the
/// question is which host wins, and it is better asked where the answer can be reported than
/// resolved by whichever connection was accepted first.
///
/// # Errors
/// [`TransportError::Carrier`] if the address cannot be bound -- the port is in use, or the
/// interface has no address yet.
pub fn listen<N: NetBackend + ?Sized>(
    net: &mut N,
    addr: &[u8],
    port: u16,
) -> Result<SocketHandle, TransportError> {
    match net.tcp_listen(addr, port, 1) {
        NetResult::Ready(listener) => Ok(listener),
        NetResult::WouldBlock | NetResult::Error => Err(TransportError::Carrier),
    }
}

/// Take one pending connection off `listener` as a session. `Ok(None)` means none is waiting yet.
///
/// # Why a broken listener is not spelled the same as a quiet one
///
/// Both are "no session right now", and folding them together is the mistake this protocol's own
/// error type was widened to avoid: a listener that has failed reports nothing forever, and a
/// board waiting on one is indistinguishable from a board nobody has connected to. One of those
/// wants patience and the other wants a restart, and a caller that cannot tell them apart will
/// pick patience every time -- which is how a dead board looks merely idle for an afternoon.
///
/// A caller holding a live session should keep calling this and [`TcpLink::close`] whatever it
/// returns, rather than leaving a second host's connection pending in the backlog. Closing it
/// tells that host it lost; leaving it queued makes it wait for a session it will never be given.
///
/// # Errors
/// [`TransportError::Carrier`] if the listener itself failed.
pub fn accept_link<N: NetBackend + ?Sized>(
    net: &mut N,
    listener: SocketHandle,
) -> Result<Option<TcpLink>, TransportError> {
    match net.accept(listener) {
        NetResult::Ready(socket) => Ok(Some(TcpLink::new(socket))),
        NetResult::WouldBlock => Ok(None),
        NetResult::Error => Err(TransportError::Carrier),
    }
}

/// A [`TcpLink`] that owns its backend, so it is a plain [`Transport`] the runner and the debug
/// agent take unchanged.
///
/// This is the right shape when the wire is the only thing on the board using the network. When it
/// is not -- when managed code opens sockets against the same stack -- hold a [`TcpLink`] beside
/// the shared backend instead and pass the backend in at each call. The two are the same
/// implementation; only the ownership differs.
pub struct TcpTransport<N: NetBackend> {
    net: N,
    link: TcpLink,
}

impl<N: NetBackend> TcpTransport<N> {
    /// Wrap an already-connected socket and the backend it belongs to.
    pub fn new(net: N, socket: SocketHandle) -> Self {
        Self { net, link: TcpLink::new(socket) }
    }

    /// Wrap an established session and the backend it belongs to.
    pub fn from_link(net: N, link: TcpLink) -> Self {
        Self { net, link }
    }

    /// The session underneath, for the operations that are not part of the [`Transport`] seam:
    /// the pending-byte count, the readiness registration, the close.
    pub fn link(&mut self) -> &mut TcpLink {
        &mut self.link
    }

    /// The backend underneath.
    pub fn backend(&mut self) -> &mut N {
        &mut self.net
    }

    /// Push whatever the socket has not accepted yet. `Ok(true)` when nothing is left.
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if the socket failed, or the session's ending if it already had
    /// one -- see [`TcpLink::flush`], which this is.
    pub fn flush(&mut self) -> Result<bool, TransportError> {
        self.link.flush(&mut self.net)
    }

    /// Close the socket and end the session.
    pub fn close(&mut self) {
        self.link.close(&mut self.net);
    }
}

impl<N: NetBackend> Transport for TcpTransport<N> {
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        self.link.send_frame(&mut self.net, msg_type, seq, payload)
    }

    fn poll(&mut self) -> Result<Option<Frame>, TransportError> {
        self.link.poll_frame(&mut self.net)
    }
}
