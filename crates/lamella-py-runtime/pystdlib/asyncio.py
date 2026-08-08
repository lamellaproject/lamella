# asyncio, bundled as a MANAGED module over the shared block point.
#
# The split is the point. `lamella-reactor` owns the IDLE ALGORITHM -- with nothing runnable, block the
# OS thread ONCE on the nearest deadline and report who to wake -- and it is shared with the C# tier's
# scheduler and the AOT one, so a sleep parks in the SAME wait on every tier of one board. What lives
# here is the READY QUEUE and everything above it: futures, tasks, callbacks. Those are written in
# Python because a program reads them; `_reactor` is native because parking a thread is not something
# Python can express.
#
# A waiter is an opaque integer on both sides of that seam. Nothing below this file knows what a
# coroutine is, which is what keeps a language's concurrency model out of the substrate.
#
# HOW SUSPENSION ACTUALLY HAPPENS, since it is spread over three layers and is hard to see in any one:
# `Future.__await__` yields SELF while the result is not ready. That yield leaves the awaiting
# coroutine's frame suspended and travels outward through every enclosing `await` -- each re-yields it
# -- until it reaches the `_coro.send(None)` in `Task._step`, which is what resumed the outermost
# coroutine. So what a task receives is exactly the future its chain is blocked on, and the task's only
# job is to arrange to be stepped again when that future completes.
#
# NOT PROVIDED, and each is absent rather than stubbed: the network surface (`open_connection`,
# `start_server`, the transport/protocol layer) -- this runtime has no sockets, so the reactor's poll
# set is empty by construction; subprocesses and threads (`to_thread`, `run_in_executor`); the
# synchronization primitives (`Lock`, `Event`, `Semaphore`, `Queue`); `wait_for`, `wait`, `shield`,
# `timeout`; and async generators, which the front end refuses because they are a third kind of code
# object it does not encode.
import _reactor

#: Milliseconds per second -- the seam speaks milliseconds and asyncio's public surface speaks seconds
#: as floats, so the conversion lives in one place rather than at each call.
_MS_PER_SECOND = 1000

#: The loop `get_event_loop()` hands out, created on first ask. One per program: this runtime has one
#: OS thread and the block point is a property of that thread, so a second loop would be a second
#: claim on the same wait.
_loop = None


class CancelledError(BaseException):
    # A BaseException rather than an Exception, as in CPython 3.8 and later: cancellation must not be
    # swallowed by an `except Exception` written to catch a task's own errors.
    pass


class InvalidStateError(Exception):
    pass


class Future:
    # A result that is not there yet, and the thing an `await` actually suspends on.
    #
    # Settable ONCE: `set_result`/`set_exception` refuse a future that is already done, because two
    # results mean the second one is invisible to whoever already resumed on the first.

    def __init__(self, loop=None):
        self._loop = loop if loop is not None else get_event_loop()
        self._done = False
        self._cancelled = False
        self._result = None
        self._exception = None
        self._callbacks = []

    def done(self):
        return self._done

    def cancelled(self):
        return self._cancelled

    def result(self):
        if self._cancelled:
            raise CancelledError()
        if not self._done:
            raise InvalidStateError("Result is not set.")
        if self._exception is not None:
            raise self._exception
        return self._result

    def exception(self):
        if self._cancelled:
            raise CancelledError()
        if not self._done:
            raise InvalidStateError("Exception is not set.")
        return self._exception

    def set_result(self, result):
        if self._done:
            raise InvalidStateError("invalid state")
        self._result = result
        self._done = True
        self._schedule_callbacks()

    def set_exception(self, exception):
        if self._done:
            raise InvalidStateError("invalid state")
        if isinstance(exception, type):
            exception = exception()
        self._exception = exception
        self._done = True
        self._schedule_callbacks()

    def cancel(self):
        # A future already settled cannot be cancelled -- its result has been observed, or is about to
        # be. Answering False rather than raising is what lets a caller cancel speculatively.
        if self._done:
            return False
        self._cancelled = True
        self._done = True
        self._exception = CancelledError()
        self._schedule_callbacks()
        return True

    def add_done_callback(self, callback):
        # A callback is never run inline, even when the future is ALREADY done: it goes on the ready
        # queue. Running it here would run it beneath whatever is currently executing, which is the
        # re-entrancy an event loop exists to remove.
        if self._done:
            self._loop.call_soon(callback, self)
        else:
            self._callbacks.append(callback)

    def _schedule_callbacks(self):
        callbacks = self._callbacks
        self._callbacks = []
        for callback in callbacks:
            self._loop.call_soon(callback, self)

    def __await__(self):
        # >>> THE SUSPENSION POINT. Yielding self hands this future to the task driving the chain,
        # which is the only participant that can arrange for the chain to resume. <<<
        if not self._done:
            yield self
        if not self._done:
            raise RuntimeError("await wasn't used with future")
        return self.result()


class Task(Future):
    # A coroutine being driven by the loop, wearing a Future's face so it can be awaited and gathered.
    #
    # It is a Future whose result is what the coroutine returns, and the loop's unit of scheduling: the
    # only place `send` is called on a coroutine is `_step` below.

    def __init__(self, coro, loop=None):
        Future.__init__(self, loop)
        self._coro = coro
        self._awaiting = None
        self._must_cancel = False
        self._loop.call_soon(self._step)

    def _step(self, _future=None):
        self._awaiting = None
        try:
            if self._must_cancel:
                self._must_cancel = False
                sent = self._coro.throw(CancelledError())
            else:
                sent = self._coro.send(None)
        except StopIteration as stop:
            # The coroutine returned. `Future.set_result` rather than `self.set_result`, so that a task
            # completing is not routed through the refusal below.
            Future.set_result(self, stop.value)
            return
        except CancelledError:
            Future.cancel(self)
            return
        except BaseException as error:
            Future.set_exception(self, error)
            return
        # The coroutine suspended, and what came out is what it is waiting on.
        if sent is None:
            # A bare yield: "let something else run". `sleep(0)` is spelled this way, so a cooperative
            # loop can hand over without involving a timer at all.
            self._loop.call_soon(self._step)
        elif isinstance(sent, Future):
            if sent is self:
                Future.set_exception(self, RuntimeError("Task cannot await on itself"))
                return
            self._awaiting = sent
            sent.add_done_callback(self._step)
        else:
            # Anything else means an `__await__` yielded something the loop has no way to wait for.
            # Naming what came out is the difference between a fixable report and a hang.
            message = "Task got bad yield: " + repr(sent)
            Future.set_exception(self, RuntimeError(message))

    def cancel(self):
        if self._done:
            return False
        # Cancelling a task is not cancelling its future: the coroutine is mid-flight and has `finally`
        # blocks owed to it, so the CancelledError is THROWN IN at the point it suspended and the task
        # settles only when the coroutine actually unwinds.
        awaiting = self._awaiting
        if awaiting is not None and awaiting.cancel():
            return True
        self._must_cancel = True
        return True

    def set_result(self, result):
        raise RuntimeError("Task does not support set_result operation")

    def set_exception(self, exception):
        raise RuntimeError("Task does not support set_exception operation")


class TimerHandle:
    # What `call_later` hands back, so a scheduled callback can be called off. Cancelling drops the
    # loop's timer entry AND the reactor's park, because a park nothing will act on still shapes the
    # next block point's wait.

    def __init__(self, loop, waiter):
        self._loop = loop
        self._waiter = waiter
        self._cancelled = False

    def cancelled(self):
        return self._cancelled

    def cancel(self):
        if self._cancelled:
            return
        self._cancelled = True
        self._loop._cancel_timer(self._waiter)


class EventLoop:
    # The ready queue, and the one place this runtime blocks.
    #
    # Two stores, and the split is the whole design: `_ready` is what can run NOW, and `_timers` is
    # what is waiting for a moment. A pass runs everything ready and only then, with nothing runnable,
    # asks the reactor for the single blocking wait.

    def __init__(self):
        self._ready = []
        self._timers = {}
        self._next_waiter = 1
        self._running = False

    def is_running(self):
        return self._running

    def create_future(self):
        return Future(self)

    def create_task(self, coro):
        return Task(coro, self)

    def call_soon(self, callback, *args):
        self._ready.append((callback, args))

    def call_later(self, delay, callback, *args):
        delay_ms = int(delay * _MS_PER_SECOND)
        if delay_ms < 0:
            delay_ms = 0
        waiter = self._next_waiter
        self._next_waiter = waiter + 1
        deadline = _reactor.now_ms() + delay_ms
        self._timers[waiter] = (deadline, callback, args)
        _reactor.park(waiter, deadline)
        return TimerHandle(self, waiter)

    def _cancel_timer(self, waiter):
        if waiter in self._timers:
            del self._timers[waiter]
        _reactor.unpark(waiter)

    def run_until_complete(self, future):
        if self._running:
            raise RuntimeError("This event loop is already running")
        self._running = True
        try:
            while not future.done():
                if not self._run_once():
                    # The reactor reported nothing left to wait for while work is still pending. That
                    # is a deadlock, and saying so beats CPython's behavior of waiting forever: a
                    # program that can never finish is more useful stopped with a reason.
                    raise RuntimeError("Event loop stopped before Future completed")
        finally:
            self._running = False
        return future.result()

    def _run_once(self):
        # One pass. False = nothing ran and nothing can, which is the only honest way to stop.
        if self._ready:
            # A SNAPSHOT: callbacks scheduled by this batch run on the NEXT pass. Draining as we go
            # would let two callbacks that reschedule each other starve every timer forever.
            batch = self._ready
            self._ready = []
            for callback, args in batch:
                callback(*args)
            return True
        if not self._timers:
            return False
        woken = _reactor.block_point()
        if woken is None:
            return False
        due = []
        for waiter in woken:
            if waiter in self._timers:
                due.append(waiter)
        # NEAREST DEADLINE FIRST, and it earns its keep on a host with NO CLOCK. There the reactor
        # treats every timer as already due and hands them all back at once, so ORDER is the only part
        # of a delay that can still be honored -- and with an origin of zero a deadline IS the delay
        # that asked for it. With a real clock the block point returns only what is genuinely due and
        # this reorders a handful of entries, which costs nothing.
        while due:
            nearest = 0
            index = 1
            while index < len(due):
                if self._timers[due[index]][0] < self._timers[due[nearest]][0]:
                    nearest = index
                index += 1
            waiter = due.pop(nearest)
            entry = self._timers[waiter]
            del self._timers[waiter]
            entry[1](*entry[2])
        return True


def get_event_loop():
    global _loop
    if _loop is None:
        _loop = EventLoop()
    return _loop


def get_running_loop():
    loop = get_event_loop()
    if not loop.is_running():
        raise RuntimeError("no running event loop")
    return loop


def new_event_loop():
    return EventLoop()


def set_event_loop(loop):
    global _loop
    _loop = loop


def iscoroutine(obj):
    # A coroutine is not a class a program can name here, so this asks what it IS. The type's name is
    # `coroutine`, exactly as in CPython, and it is the same object identity `type(f())` gives.
    return type(obj).__name__ == "coroutine"


def isfuture(obj):
    return isinstance(obj, Future)


def ensure_future(obj, loop=None):
    # A coroutine becomes a Task; a Future is already one. Everything the loop waits on is a Future, so
    # this is where the two spellings a caller may use converge.
    if isfuture(obj):
        return obj
    if iscoroutine(obj):
        return Task(obj, loop)
    raise TypeError("An asyncio.Future, a coroutine or an awaitable is required")


def create_task(coro):
    if not iscoroutine(coro):
        raise TypeError("a coroutine was expected, got " + repr(coro))
    return get_running_loop().create_task(coro)


class _YieldOnce:
    # `sleep(0)`'s awaitable: a bare yield, which the task reads as "reschedule me". No future and no
    # timer, because "let something else run" is not a thing to WAIT for -- and going through a timer
    # would park a deadline that is already past, then block on it.
    def __await__(self):
        yield None


def _set_result_unless_done(future, result):
    if not future.done():
        future.set_result(result)


async def sleep(delay, result=None):
    if delay <= 0:
        await _YieldOnce()
        return result
    loop = get_running_loop()
    future = loop.create_future()
    handle = loop.call_later(delay, _set_result_unless_done, future, result)
    try:
        return await future
    finally:
        # The timer has normally fired by now, so this is for the path where it has not: the awaiting
        # task was cancelled, and a park left behind would shape a later block point's wait.
        handle.cancel()


async def gather(*aws):
    # Every awaitable runs CONCURRENTLY -- each becomes a task before any of them is waited on, which
    # is what separates this from awaiting them one after another. Results come back in the order the
    # arguments were given, never in completion order.
    children = []
    for aw in aws:
        children.append(ensure_future(aw))
    if not children:
        return []
    loop = get_running_loop()
    outcome = loop.create_future()
    results = []
    for _ in children:
        results.append(None)
    # A one-element list rather than a plain integer: the callback below rebinds nothing in this
    # frame, so the count has to live somewhere both can reach.
    remaining = [len(children)]

    def _child_done(child):
        index = 0
        while index < len(children):
            if children[index] is child:
                break
            index += 1
        if outcome.done():
            return
        error = child.exception()
        if error is not None:
            # The FIRST failure settles the gather; the others keep running but their results are no
            # longer anybody's. That is CPython's default (`return_exceptions=False`), which is the
            # only mode offered here.
            outcome.set_exception(error)
            return
        results[index] = child.result()
        remaining[0] = remaining[0] - 1
        if remaining[0] == 0:
            outcome.set_result(results)

    for child in children:
        child.add_done_callback(_child_done)
    return await outcome


def run(main):
    # The entry point: drive `main` to completion on a loop that belongs to this call.
    #
    # CPython refuses a nested `run()` and so does this -- the block point is a property of the one OS
    # thread, so a second loop inside the first would be two schedulers claiming one wait.
    if not iscoroutine(main):
        raise TypeError("An asyncio.Future, a coroutine or an awaitable is required")
    loop = get_event_loop()
    if loop.is_running():
        raise RuntimeError("asyncio.run() cannot be called from a running event loop")
    return loop.run_until_complete(loop.create_task(main))
