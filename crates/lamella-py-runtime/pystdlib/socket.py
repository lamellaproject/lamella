# socket, bundled as a MANAGED module over the native `_socket` seam.
#
# The same split CPython uses: `_socket` is the smallest thing that must be native (reaching the
# embedder's network backend) and everything a program actually calls is built here in Python -- the
# socket object, the address tuples, the constants, sendall's loop, and the context manager.
#
# THE SEMANTICS BELOW ARE INHERITED, NOT DECIDED. The backend is non-blocking and reports
# would-block, a clean peer close, and a genuine error as three different things; the native layer
# parks on the shared reactor so these calls block the way CPython's do. In particular a `recv` that
# returns b"" means THE PEER CLOSED CLEANLY -- it never means "nothing yet", which is a park. Two
# language runtimes on one board have to agree about that, so this module does not get a say in it.
#
# THE TIMEOUT SURFACE IS HERE NOW, and it is the one place Python's meaning of a value differs from
# the seam's. `settimeout(0)` IS `setblocking(False)` in CPython -- the same call -- so zero means
# NEVER WAIT, while the shared deadline table reads zero as CLEAR (wait forever). The translation
# lives once, in the native layer, with the measurement quoted at it; nothing in this file encodes
# it, so a reader cannot half-apply it.
#
# WHICH EXCEPTION YOU GET IS THE MODE, NOT THE CLOCK. A non-blocking socket raises `BlockingIOError`
# (normal, retry -- the select-loop idiom); a socket with a timeout raises `TimeoutError` (the peer
# was too slow). Both subclass `OSError`, so catching that catches either, but a retry loop written
# against `BlockingIOError` will NOT catch a `TimeoutError`.
#
# NOT PROVIDED: socketpair, the SO_* option surface (setsockopt/getsockopt), makefile, and AF_UNIX.
# Each needs a backend capability the seam does not carry, and a stub that accepted the call and
# ignored it would be worse than its absence.
import _socket

# Address families and socket types. The values are the BSD/POSIX ones every platform agrees on, so
# a program that prints or compares them sees what it sees on CPython.
AF_INET = 2
AF_INET6 = 23
SOCK_STREAM = 1
SOCK_DGRAM = 2

# What `listen()` passes when a caller does not choose, matching CPython's default backlog.
_DEFAULT_BACKLOG = 128


def _pack_ipv4(text):
    # A dotted quad to 4 bytes. Returns None when `text` is not one, so the caller can fall back to
    # resolving it as a name -- "10.0.0.1" must never take a trip through a resolver.
    parts = text.split(".")
    if len(parts) != 4:
        return None
    octets = []
    for part in parts:
        if not part or not part.isdigit():
            return None
        value = int(part)
        if value > 255:
            return None
        octets.append(value)
    return bytes(octets)


def inet_aton(text):
    packed = _pack_ipv4(text)
    if packed is None:
        raise OSError("illegal IP address string passed to inet_aton")
    return packed


def inet_ntoa(packed):
    if len(packed) != 4:
        raise OSError("packed IP wrong length for inet_ntoa")
    return ".".join([str(b) for b in packed])


def _resolve_one(host):
    # A literal address is used as-is; anything else goes to the backend's resolver and the FIRST
    # answer is taken, which is what `gethostbyname` promises.
    packed = _pack_ipv4(host)
    if packed is not None:
        return packed
    found = _socket.resolve(host)
    if not found:
        raise OSError("getaddrinfo failed for " + host)
    return found[0]


def gethostbyname(host):
    return inet_ntoa(_resolve_one(host))


def _split_address(address):
    # CPython's address tuple for AF_INET: (host, port).
    if not isinstance(address, tuple) or len(address) != 2:
        raise TypeError("AF_INET address must be a (host, port) tuple")
    host, port = address
    if not isinstance(port, int) or isinstance(port, bool):
        raise TypeError("port must be an integer")
    return _resolve_one(host), port


class socket:
    def __init__(self, family=AF_INET, type=SOCK_STREAM):
        if family != AF_INET:
            raise OSError("only AF_INET is supported by this runtime")
        if type != SOCK_STREAM and type != SOCK_DGRAM:
            raise OSError("only SOCK_STREAM and SOCK_DGRAM are supported by this runtime")
        self.family = family
        self.type = type
        # The backend hands out a handle when the socket is actually opened, which for a TCP client
        # is at connect() -- so an unconnected socket has no handle rather than a placeholder one.
        self._handle = None
        # None = block indefinitely, CPython's default for a fresh socket. Held here until a handle
        # exists to key it against; see settimeout.
        self._timeout = None

    # ----- lifetime -----

    def close(self):
        if self._handle is not None:
            _socket.close(self._handle)
            self._handle = None

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_value, traceback):
        self.close()
        return False

    # ----- blocking mode and timeouts -----

    def settimeout(self, value):
        # CPython's tri-state, passed through unchanged so the ONE translation stays in one place:
        # None blocks indefinitely, 0 means never wait, a positive value is a deadline in seconds.
        #
        # IT MUST WORK BEFORE THERE IS A HANDLE, because the commonest use of this call is timing out
        # a CONNECT -- `s = socket(); s.settimeout(5); s.connect(addr)` -- and the backend hands out
        # a handle only when the socket is actually opened. So the value is held here and applied at
        # the moment a handle appears, the same way a pending bind is. Requiring a handle would have
        # refused exactly the case the API exists for.
        self._timeout = value
        if self._handle is not None:
            _socket.set_timeout(self._handle, value)

    def gettimeout(self):
        # Answered from the handle when there is one, so a value the native layer clamped (a positive
        # timeout too small for the seam to express) reads back as what will actually happen rather
        # than as what was asked for. Before that, the held value is all there is.
        if self._handle is None:
            return getattr(self, "_timeout", None)
        return _socket.get_timeout(self._handle)

    def _apply_pending_timeout(self):
        # Called by every path that obtains a handle. A no-op for a socket nobody set a timeout on.
        pending = getattr(self, "_timeout", None)
        if pending is not None and self._handle is not None:
            _socket.set_timeout(self._handle, pending)

    def setblocking(self, flag):
        # CPython defines this in terms of settimeout rather than beside it: `setblocking(True)` IS
        # `settimeout(None)` and `setblocking(False)` IS `settimeout(0)`. Writing it that way here
        # means the two surfaces cannot drift apart, and that `getblocking` below stays consistent
        # with `gettimeout` by construction rather than by agreement.
        self.settimeout(None if flag else 0)

    def getblocking(self):
        # True unless a timeout of exactly zero put the socket in non-blocking mode. A socket with a
        # POSITIVE timeout is still blocking -- it blocks, it just gives up eventually -- which is
        # the part of this API that most often reads backwards.
        return self.gettimeout() != 0

    def fileno(self):
        # The backend's handle, or -1 for a socket that has none -- CPython's answer for a closed one.
        return -1 if self._handle is None else self._handle

    def _require(self):
        if self._handle is None:
            raise OSError("socket is not connected or bound")
        return self._handle

    # ----- TCP client -----

    def connect(self, address):
        if self._handle is not None:
            raise OSError("socket is already connected")
        addr, port = _split_address(address)
        if getattr(self, "_timeout", None) == 0:
            # >>> A NON-BLOCKING CONNECT STARTS AND REPORTS ITSELF UNFINISHED, which is CPython's
            # behaviour (`BlockingIOError`, EINPROGRESS / WSAEWOULDBLOCK) and not a limitation of this
            # runtime. The caller waits for WRITABILITY and then calls `connect_check`; that is what
            # `asyncio.open_connection` does, and it is the only way a connect does not stop
            # everything else the program is doing. <<<
            #
            # It has to be decided HERE rather than inside the seam: a socket's mode is keyed by its
            # handle, and the blocking `tcp_connect` below does not return one until the wait it would
            # have skipped is already over.
            self._handle = _socket.tcp_connect_start(addr, port)
            self._apply_pending_timeout()
            if not _socket.connect_check(self._handle):
                raise BlockingIOError("connect in progress")
            return
        self._handle = _socket.tcp_connect(addr, port)
        self._apply_pending_timeout()

    def connect_check(self):
        # Whether a connect started on a non-blocking socket has finished: True connected, False still
        # connecting, and it RAISES if the connect failed. What a caller runs after waiting for
        # writability; CPython spells the same question `getsockopt(SOL_SOCKET, SO_ERROR)`, which this
        # runtime has no socket-option surface for.
        return _socket.connect_check(self._require())

    def send(self, data):
        # The count actually written, which MAY BE SHORT. CPython says the same, and `sendall` is the
        # one that loops -- a send that quietly looped would delete the choice the caller made.
        return _socket.send(self._require(), bytes(data))

    def sendall(self, data):
        view = bytes(data)
        while view:
            sent = _socket.send(self._require(), view)
            if sent <= 0:
                raise OSError("connection closed while sending")
            view = view[sent:]
        return None

    def recv(self, bufsize):
        if bufsize < 0:
            raise ValueError("negative buffersize in recv")
        return _socket.recv(self._require(), bufsize)

    # ----- TCP server -----

    def bind(self, address):
        addr, port = _split_address(address)
        if self.type == SOCK_DGRAM:
            self._handle = _socket.udp_bind(addr, port)
            self._apply_pending_timeout()
        else:
            # A TCP bind is not separable from the listen at this seam, so the address is held and
            # the socket is opened by listen(). Stated rather than hidden: a program that binds and
            # never listens has reserved nothing, and would find that out at the next bind.
            self._pending_bind = (addr, port)

    def listen(self, backlog=_DEFAULT_BACKLOG):
        addr, port = getattr(self, "_pending_bind", (bytes([0, 0, 0, 0]), 0))
        self._handle = _socket.tcp_listen(addr, port, backlog)
        self._apply_pending_timeout()

    def accept(self):
        accepted = _socket.accept(self._require())
        peer = socket(self.family, self.type)
        peer._handle = accepted
        # CPython returns (conn, address). The seam does not report the peer's address on accept, so
        # the port is answered honestly as unknown rather than invented.
        return peer, ("", 0)

    def getsockname(self):
        port = _socket.local_port(self._require())
        return ("", 0 if port is None else port)

    # ----- UDP -----

    def sendto(self, data, address):
        addr, port = _split_address(address)
        if self._handle is None:
            self._handle = _socket.udp_bind(bytes([0, 0, 0, 0]), 0)
        return _socket.udp_send_to(self._handle, bytes(data), addr, port)

    def recvfrom(self, bufsize):
        if bufsize < 0:
            raise ValueError("negative buffersize in recvfrom")
        data, addr, port = _socket.udp_recv_from(self._require(), bufsize)
        return data, (inet_ntoa(addr) if len(addr) == 4 else "", port)


def create_connection(address, timeout=None):
    # CPython's convenience constructor. `timeout` used to be accepted and REFUSED here, because a
    # connection that silently never times out is the failure this signature is usually reached for.
    # There is a timeout surface now, so it is applied instead.
    made = socket(AF_INET, SOCK_STREAM)
    made.connect(address)
    if timeout is not None:
        made.settimeout(timeout)
    return made
