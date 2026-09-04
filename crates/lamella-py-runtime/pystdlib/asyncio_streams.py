# asyncio's STREAM surface -- the reader/writer pair over a socket, and the two ways to get one.
#
# THIS FILE IS APPENDED TO `asyncio.py` when the `asyncio-streams` knob is on, so what it defines
# lands in the `asyncio` module itself: `asyncio.open_connection`, `asyncio.start_server`. Same
# reason as the coordination layer -- CPython puts these names in `asyncio`, and a program that
# imports them from anywhere else stops being a program CPython can run.
#
# A SEPARATE KNOB FROM `asyncio-sync`, because they are different axes and a board should not have
# to take both. Coordination is useful on a part with no network at all; streams are useless on one,
# since every call here ends at a socket the embedder has to have supplied a backend for.
#
# WHY THIS IS NOT JUST `socket` WITH `await` IN FRONT. A blocking `recv` parks the OS THREAD, and
# this runtime has one -- so a loop that let a task do that would stop every other task until the
# peer sent something. The whole design is the avoidance of that:
#
#     the socket is NON-BLOCKING       so a read that has nothing yet raises BlockingIOError
#                                      instead of parking the thread
#     the TASK parks, not the thread   `loop._add_reader` registers the socket with the reactor and
#                                      suspends this coroutine; other tasks keep running
#     ONE block point serves both      the reactor waits on the nearest timer deadline AND every
#                                      armed socket in a single wait, so a device with a sleeping
#                                      task and a listening socket still idles rather than spins
#
# `socket` is imported as a MODULE rather than reached through the raw seam, because what is being
# reused is a rule and not a function: address parsing is going to gain cases (a literal that is not
# a dotted quad, IPv6), and a second implementation of a rule is the defect that arrives later in
# whichever copy nobody touched.
import socket as _socketmod

#: What one read asks the socket for, and the most a reader buffers ahead of what was asked. 64 KiB
#: is CPython's default; this is smaller because a device's arena is, and because a stream that
#: buffers more than the program has asked for is holding memory on the program's behalf without
#: being told to.
_STREAM_LIMIT = 8192


class IncompleteReadError(EOFError):
    # What `readexactly` and `readuntil` raise when the stream ends before the request could be met.
    # It carries the PARTIAL data, because a caller that asked for 40 bytes and can have 12 usually
    # wants to see the 12 -- a bare EOF throws away the only evidence of what went wrong.

    def __init__(self, partial, expected):
        EOFError.__init__(self, "%d bytes read on a total of %s expected bytes"
                          % (len(partial), expected))
        self.partial = partial
        self.expected = expected


class LimitOverrunError(Exception):
    # `readuntil` found no separator within the limit. Separate from IncompleteReadError because the
    # stream is FINE -- it is the caller's expectation that was wrong, and the data is still there.

    def __init__(self, message, consumed):
        Exception.__init__(self, message)
        self.consumed = consumed


async def _wait_ready(loop, sock, writable=False):
    # >>> THE ONE PLACE A STREAM WAITS, AND IT SUSPENDS THE TASK RATHER THAN THE THREAD. <<<
    future = loop.create_future()
    waiter = loop._add_reader(sock, lambda: _set_result_unless_done(future, None), writable)
    try:
        await future
    except CancelledError:
        # The registration outlives the await if nobody drops it, and a socket left armed makes the
        # reactor wait on readiness no task is going to act on.
        loop._cancel_io(waiter)
        raise


class StreamReader:
    # The reading half. Buffers what arrived, hands back what was asked for, and only touches the
    # socket when the buffer cannot answer.

    def __init__(self, sock, loop=None, limit=_STREAM_LIMIT):
        self._sock = sock
        self._loop = loop if loop is not None else get_event_loop()
        self._limit = limit
        self._buffer = b""
        self._eof = False

    def at_eof(self):
        # True only when the buffer is ALSO empty: a stream whose peer has gone but whose bytes have
        # not been read yet is not at its end, and a reader loop written against this would drop the
        # last chunk if it were.
        return self._eof and not self._buffer

    def feed_data(self, data):
        # For a reader driven by something other than its socket -- a test, or a protocol that has
        # already read the bytes. CPython exposes it for the same reason.
        if data:
            self._buffer = self._buffer + data

    def feed_eof(self):
        self._eof = True

    async def _fill(self):
        # One socket read into the buffer. Returns how many bytes arrived; 0 means the peer closed.
        if self._eof:
            return 0
        while True:
            try:
                chunk = self._sock.recv(self._limit)
            except BlockingIOError:
                # Nothing yet. Park the TASK on readability and come back.
                await _wait_ready(self._loop, self._sock)
                continue
            break
        if not chunk:
            # An EMPTY read is a clean peer close, not "nothing yet" -- that was the BlockingIOError
            # above. The seam draws the same line, so the two agree without this layer deciding it.
            self._eof = True
            return 0
        self._buffer = self._buffer + chunk
        return len(chunk)

    def _take(self, count):
        taken = self._buffer[:count]
        self._buffer = self._buffer[count:]
        return taken

    async def read(self, n=-1):
        # `n < 0` reads to EOF; otherwise UP TO n bytes, which may be fewer. Returning short is not a
        # defect: a stream that waited for the full n would deadlock against a peer that is waiting
        # for a reply to what it already sent.
        if n == 0:
            return b""
        if n < 0:
            while await self._fill():
                pass
            return self._take(len(self._buffer))
        while not self._buffer and not self._eof:
            await self._fill()
        return self._take(n)

    async def readexactly(self, n):
        # EXACTLY n, or IncompleteReadError carrying what there was.
        if n < 0:
            raise ValueError("readexactly size can not be less than zero")
        while len(self._buffer) < n:
            if not await self._fill():
                partial = self._take(len(self._buffer))
                raise IncompleteReadError(partial, n)
        return self._take(n)

    async def readuntil(self, separator=b"\n"):
        # Up to AND INCLUDING the separator.
        if not separator:
            raise ValueError("Separator should be at least one-byte string")
        start = 0
        while True:
            index = self._buffer.find(separator, start)
            if index >= 0:
                return self._take(index + len(separator))
            if len(self._buffer) > self._limit:
                raise LimitOverrunError("Separator is not found, and chunk exceed the limit",
                                        len(self._buffer))
            # Only the tail can complete a separator that straddles two reads, so the next search
            # starts where a partial match could still begin rather than from zero.
            start = len(self._buffer) - len(separator) + 1
            if start < 0:
                start = 0
            if not await self._fill():
                partial = self._take(len(self._buffer))
                raise IncompleteReadError(partial, None)

    async def readline(self):
        # A line, or what there was when the stream ended -- and ending mid-line is NOT an error here,
        # unlike `readuntil`. CPython draws that line and it is the right one: a file whose last line
        # has no newline is ordinary, and a reader loop should end rather than raise on it.
        try:
            return await self.readuntil(b"\n")
        except IncompleteReadError as partial:
            return partial.partial
        except LimitOverrunError as overrun:
            raise ValueError(str(overrun))

    def __aiter__(self):
        return self

    async def __anext__(self):
        # `async for line in reader:` -- the shape CPython added and the one a reader loop wants.
        line = await self.readline()
        if not line:
            raise StopAsyncIteration()
        return line


class StreamWriter:
    # The writing half. `write` BUFFERS and does not block; `drain` is where a caller waits for the
    # peer to keep up. That split is CPython's and it is what lets a producer write a burst without
    # a round trip per chunk.

    def __init__(self, sock, reader=None, loop=None):
        self._sock = sock
        self._reader = reader
        self._loop = loop if loop is not None else get_event_loop()
        self._pending = b""
        self._closing = False
        self._closed = Future(self._loop)

    def write(self, data):
        if self._closing:
            raise RuntimeError("write() called on a closing stream")
        self._pending = self._pending + bytes(data)

    def writelines(self, chunks):
        for chunk in chunks:
            self.write(chunk)

    async def drain(self):
        # >>> WHERE BACKPRESSURE LIVES. Without an await here a fast producer buffers without bound
        # and the arena is what runs out -- on a device, long before the peer complains. <<<
        while self._pending:
            try:
                sent = self._sock.send(self._pending)
            except BlockingIOError:
                await _wait_ready(self._loop, self._sock, True)
                continue
            if sent <= 0:
                raise OSError("connection closed while sending")
            self._pending = self._pending[sent:]

    def can_write_eof(self):
        return False

    def is_closing(self):
        return self._closing

    def close(self):
        # Does NOT flush: a caller that wants the pending bytes delivered awaits `drain()` first,
        # which is CPython's contract too. Closing here and flushing implicitly would make an
        # unawaited close look reliable and fail only under load.
        if self._closing:
            return
        self._closing = True
        self._sock.close()
        _set_result_unless_done(self._closed, None)

    async def wait_closed(self):
        await self._closed

    def get_extra_info(self, name, default=None):
        if name == "socket":
            return self._sock
        if name == "peername":
            return getattr(self._sock, "_peername", default)
        if name == "sockname":
            try:
                return self._sock.getsockname()
            except OSError:
                return default
        return default


def _wrap(sock, loop):
    # The pair every entry point here hands back.
    reader = StreamReader(sock, loop)
    return reader, StreamWriter(sock, reader, loop)


async def open_connection(host, port, limit=_STREAM_LIMIT):
    # Connects and returns `(reader, writer)`.
    loop = get_event_loop()
    sock = _socketmod.socket()
    sock.setblocking(False)
    try:
        sock.connect((host, port))
    except BlockingIOError:
        # >>> THE HANDSHAKE, WAITED FOR WITHOUT STOPPING THE LOOP. A connect is the one socket
        # operation with no data to be ready for, so the readiness asked about is WRITABILITY -- and
        # `connect_check` is what turns "the socket became writable" into "the connect succeeded",
        # since a failed connect makes it writable too. <<<
        while True:
            await _wait_ready(loop, sock, True)
            if sock.connect_check():
                break
    sock._peername = (host, port)
    reader, writer = _wrap(sock, loop)
    reader._limit = limit
    return reader, writer


class Server:
    # What `start_server` hands back: the listening socket, and the accept loop driving it.

    def __init__(self, sock, handler, loop):
        self._sock = sock
        self._handler = handler
        self._loop = loop
        self._serving = None
        self._closed = Future(loop)

    @property
    def sockets(self):
        return [self._sock]

    def is_serving(self):
        return self._serving is not None and not self._serving.done()

    async def _accept_loop(self):
        while True:
            try:
                conn = self._sock.accept()
            except BlockingIOError:
                await _wait_ready(self._loop, self._sock)
                continue
            except OSError:
                # The listener was closed under us, which is how `close()` ends this loop.
                return
            client = conn[0] if isinstance(conn, tuple) else conn
            client.setblocking(False)
            reader, writer = _wrap(client, self._loop)
            # A TASK PER CONNECTION, not an await: awaiting the handler here would serve one client
            # at a time while the listener sat idle, which is the shape a server exists not to have.
            self._loop.create_task(self._handler(reader, writer))

    def close(self):
        if self._serving is not None:
            self._serving.cancel()
            self._serving = None
        self._sock.close()
        _set_result_unless_done(self._closed, None)

    async def wait_closed(self):
        await self._closed


async def start_server(client_connected_cb, host=None, port=0, backlog=100):
    # Listens, and runs `client_connected_cb(reader, writer)` as a task per connection.
    loop = get_event_loop()
    sock = _socketmod.socket()
    sock.bind((host if host is not None else "0.0.0.0", port))
    sock.listen(backlog)
    sock.setblocking(False)
    server = Server(sock, client_connected_cb, loop)
    server._serving = loop.create_task(server._accept_loop())
    return server
