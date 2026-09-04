# asyncio's SYNCHRONIZATION PRIMITIVES and its two timeouts -- the coordination layer above the loop.
#
# THIS FILE IS APPENDED TO `asyncio.py` when the `asyncio-sync` knob is on, so what it defines lands
# in the `asyncio` module itself: `asyncio.Lock`, `asyncio.Queue`, `asyncio.wait_for`. It is a
# separate file rather than a separate MODULE because CPython puts these names in `asyncio` and a
# program that imports them from anywhere else stops being a program CPython can run.
#
# WHY IT IS A KNOB AT ALL. Importing a module builds every class and function in it as a heap
# object, so everything here is paid by a program that imports asyncio whether or not it locks
# anything -- and this file is roughly two and a half times the size of the core it sits on.
#
# The smallest tier this runtime ships to reserves a 4,096-byte object heap. The core alone fits it
# with room; the core plus this file does not. That is what the knob is for, and it is why adding to
# this file is cheaper than adding to `asyncio.py`.
#
# The family knobs this the same way one layer over: the C# surface separates its concurrency CORE
# from the coordination objects layered on it, and turns the second off on small tiers while the
# first stays. What differs is the MEMBERSHIP, because the two languages draw the line in different
# places -- a lock is core surface in one and coordination surface here -- and copying the
# membership rather than the shape would put names on a tier that cannot afford them.

class Lock:
    # Mutual exclusion between coroutines, and NOT a thread lock: it never blocks the OS thread, it
    # suspends the coroutine and lets the loop run something else. That is why it is a different
    # primitive from `threading.Lock` rather than the same one imported from elsewhere -- a thread
    # lock here would stop the loop that is supposed to release it.
    #
    # FIFO, and the hand-off is what makes it fair: `release` gives the lock DIRECTLY to the first
    # waiter instead of unlocking and waking it. Unlocking first leaves a window in which a
    # coroutine that never queued can take the lock ahead of one that has been waiting, and then
    # two coroutines both believe they hold it.

    def __init__(self):
        self._locked = False
        self._waiters = []

    def locked(self):
        return self._locked

    async def acquire(self):
        # An unheld lock has no waiters -- `release` only clears the flag when the queue is empty --
        # so taking it here cannot jump a queue.
        if not self._locked:
            self._locked = True
            return True
        future = get_event_loop().create_future()
        self._waiters.append(future)
        try:
            await future
        except CancelledError:
            # Cancelled while queued. If the lock was already handed over it is ours, and nobody
            # else will release it, so pass it on rather than leaving it held forever.
            if future in self._waiters:
                self._waiters.remove(future)
            elif future.done() and not future.cancelled():
                self._release_to_next()
            raise
        # The future resolving IS the hand-off: the lock is already held on our behalf.
        return True

    def release(self):
        if not self._locked:
            raise RuntimeError("Lock is not acquired.")
        self._release_to_next()

    def _release_to_next(self):
        while self._waiters:
            future = self._waiters.pop(0)
            if not future.done():
                # The lock stays HELD and changes owner.
                future.set_result(True)
                return
        self._locked = False

    async def __aenter__(self):
        await self.acquire()
        return None

    async def __aexit__(self, exc_type, exc, tb):
        self.release()
        return False


class Event:
    # A one-to-many flag. One `set` releases every waiter, and a waiter arriving after the set does
    # not wait at all -- which is the difference between an Event and a one-slot Queue.

    def __init__(self):
        self._value = False
        self._waiters = []

    def is_set(self):
        return self._value

    def set(self):
        if self._value:
            return
        self._value = True
        waiters = self._waiters
        self._waiters = []
        for future in waiters:
            if not future.done():
                future.set_result(True)

    def clear(self):
        self._value = False

    async def wait(self):
        if self._value:
            return True
        future = get_event_loop().create_future()
        self._waiters.append(future)
        try:
            await future
        except CancelledError:
            if future in self._waiters:
                self._waiters.remove(future)
            raise
        return True


class Semaphore:
    # A counted permit. `Lock` is the same shape at one permit, and they are still two classes
    # because `Lock.release` on an unheld lock is an ERROR while `Semaphore.release` legitimately
    # raises the count -- see `BoundedSemaphore` for the case where it is not legitimate.
    #
    # The hand-off is the lock's: a released permit goes straight to the first waiter, so the count
    # never rises through zero and a late arrival cannot take a permit a waiter was promised.

    def __init__(self, value=1):
        if value < 0:
            raise ValueError("Semaphore initial value must be >= 0")
        self._value = value
        self._waiters = []

    def locked(self):
        return self._value == 0

    async def acquire(self):
        # A free permit with a queue behind it still goes to the queue.
        if self._value > 0 and not self._waiters:
            self._value -= 1
            return True
        future = get_event_loop().create_future()
        self._waiters.append(future)
        try:
            await future
        except CancelledError:
            if future in self._waiters:
                self._waiters.remove(future)
            elif future.done() and not future.cancelled():
                self._release_to_next()
            raise
        return True

    def release(self):
        self._release_to_next()

    def _release_to_next(self):
        while self._waiters:
            future = self._waiters.pop(0)
            if not future.done():
                # The permit is transferred; the count does not move.
                future.set_result(True)
                return
        self._value += 1

    async def __aenter__(self):
        await self.acquire()
        return None

    async def __aexit__(self, exc_type, exc, tb):
        self.release()
        return False


class BoundedSemaphore(Semaphore):
    # A Semaphore that refuses to be released more often than it was acquired. The extra permit a
    # plain Semaphore would grant is almost always a release in a `finally` that ran twice, and a
    # counter that silently grows turns that into unbounded concurrency far away from the mistake.

    def __init__(self, value=1):
        Semaphore.__init__(self, value)
        self._bound = value

    def release(self):
        if self._value >= self._bound and not self._waiters:
            raise ValueError("BoundedSemaphore released too many times")
        Semaphore.release(self)


class QueueEmpty(Exception):
    # `get_nowait` on an empty queue.
    pass


class QueueFull(Exception):
    # `put_nowait` on a full one.
    pass


class Queue:
    # A FIFO channel between coroutines, and the place `maxsize` earns its keep: unbounded, a fast
    # producer and a slow consumer grow the queue until the arena is gone, which on a device is the
    # failure this runtime exists to avoid.

    def __init__(self, maxsize=0):
        self._maxsize = maxsize
        self._items = []
        self._getters = []
        self._putters = []
        self._unfinished = 0
        self._finished = Event()
        self._finished.set()

    @property
    def maxsize(self):
        return self._maxsize

    def qsize(self):
        return len(self._items)

    def empty(self):
        return not self._items

    def full(self):
        # An unbounded queue is never full, and `maxsize <= 0` is how that is spelled.
        if self._maxsize <= 0:
            return False
        return len(self._items) >= self._maxsize

    def put_nowait(self, item):
        if self.full():
            raise QueueFull()
        self._put(item)
        self._unfinished += 1
        self._finished.clear()
        self._wake_next(self._getters)

    def get_nowait(self):
        if not self._items:
            raise QueueEmpty()
        item = self._get()
        self._wake_next(self._putters)
        return item

    # THE TWO HOOKS THE ORDER LIVES IN, and the only thing `LifoQueue` and `PriorityQueue` override.
    # Everything else about a queue -- the bound, the waiters, the hand-off, `join` -- is the same
    # whichever end an item leaves from, so extracting them is what stops three copies of the waiting
    # logic existing, each able to gain a fix the others do not.
    def _put(self, item):
        self._items.append(item)

    def _get(self):
        return self._items.pop(0)

    async def put(self, item):
        while self.full():
            future = get_event_loop().create_future()
            self._putters.append(future)
            try:
                await future
            except CancelledError:
                if future in self._putters:
                    self._putters.remove(future)
                raise
        self.put_nowait(item)

    async def get(self):
        # A LOOP rather than one wait: being woken means an item WAS there, and another consumer may
        # have taken it before this one was stepped.
        while not self._items:
            future = get_event_loop().create_future()
            self._getters.append(future)
            try:
                await future
            except CancelledError:
                if future in self._getters:
                    self._getters.remove(future)
                raise
        return self.get_nowait()

    def task_done(self):
        if self._unfinished <= 0:
            raise ValueError("task_done() called too many times")
        self._unfinished -= 1
        if self._unfinished == 0:
            self._finished.set()

    async def join(self):
        if self._unfinished > 0:
            await self._finished.wait()

    def _wake_next(self, waiters):
        while waiters:
            future = waiters.pop(0)
            if not future.done():
                future.set_result(None)
                return


# `asyncio.TimeoutError` IS the builtin, as in CPython 3.11 and later: one class under two names,
# so `except TimeoutError` and `except asyncio.TimeoutError` each catch what the other raises.
TimeoutError = TimeoutError


async def wait_for(aw, timeout):
    # Run `aw`, giving up after `timeout` seconds. `None` means no limit, which is the spelling that
    # lets a caller pass a configured value straight through without branching on it.
    #
    # The awaitable becomes a TASK first, because a timeout has to CANCEL something and a bare
    # coroutine has no handle to cancel. And the wait is on that task rather than a race between two
    # futures: a race leaves the loser running, which is the leak a timeout exists to prevent.
    future = ensure_future(aw)
    if timeout is None:
        return await future
    loop = get_event_loop()
    expired = []

    def _expire():
        # Only cancel something still in flight -- a task that finished on this same pass has a
        # result waiting to be read, and cancelling it would throw that away.
        if not future.done():
            expired.append(True)
            future.cancel()

    handle = loop.call_later(timeout, _expire)
    try:
        result = await future
    except CancelledError:
        # OURS or THEIRS: a cancellation we caused is a timeout, and one from outside is a
        # cancellation that must keep propagating. Without the flag the two are one exception.
        if expired:
            raise TimeoutError()
        raise
    finally:
        handle.cancel()
    return result


class Timeout:
    # The context-manager form, `async with asyncio.timeout(d)`. It cancels the TASK rather than a
    # future, because what it bounds is a BLOCK and not one await -- and cancelling a task is what
    # unwinds through every `finally` that block is owed.

    def __init__(self, when):
        self._when = when
        self._handle = None
        self._task = None
        self._expired = False

    def expired(self):
        return self._expired

    async def __aenter__(self):
        loop = get_event_loop()
        self._task = loop._current_task
        if self._task is None:
            # Outside a task there is nothing to cancel, so the block would silently never time out.
            # Saying so beats a limit that is quietly not applied.
            raise RuntimeError("timeout() must be used inside a task")
        if self._when is not None:
            self._handle = loop.call_later(self._when, self._on_timeout)
        return self

    def _on_timeout(self):
        self._expired = True
        self._task.cancel()

    async def __aexit__(self, exc_type, exc, tb):
        if self._handle is not None:
            self._handle.cancel()
            self._handle = None
        # The cancellation THIS block caused is reported to the caller as a TimeoutError; one from
        # anywhere else keeps its own identity and propagates.
        if self._expired and exc_type is not None and issubclass(exc_type, CancelledError):
            raise TimeoutError()
        return False


def timeout(delay):
    # `delay` seconds from now, or `None` for no limit.
    return Timeout(delay)


def timeout_at(when):
    # An ABSOLUTE deadline on the loop's own clock (`loop.time()`), or `None` for no limit. The
    # difference from `timeout()` is not a convenience: a deadline that survives being passed around
    # cannot be a delay, because a delay starts whenever it is finally used.
    if when is None:
        return Timeout(None)
    return Timeout(when - get_event_loop().time())


class TaskGroup:
    # STRUCTURED CONCURRENCY: a block that cannot be left while work it started is still running, and
    # that reports a child's failure AT THE FAILURE rather than whenever the program happens to end.
    #
    # WHY THIS IS THE ANSWER TO A GAP AND NOT A CONVENIENCE OVER `gather`. A task nobody awaits is
    # reported when the loop finishes -- and the shape a device runs is a loop that never finishes, so
    # on the shape that matters most the report never comes. There is no better trigger to find:
    # reporting at idle calls a task dead that the program is about to await, and a false report on
    # correct code is worse than none because it teaches a reader to skip the line that matters.
    # A TaskGroup removes the question instead of answering it later: the block does not end until its
    # children do, so a failure has somewhere to go the moment it happens. CPython's own documentation
    # now steers people here rather than to `gather` for the same reason.
    #
    # THE FOUR BEHAVIOURS, each measured against CPython 3.14.6 rather than read off a description:
    #
    #     the happy path      the block waits for every child with no explicit await, and each task's
    #                         `result()` is there afterwards
    #     a child fails       its siblings are CANCELLED, and the failure leaves the block as an
    #                         ExceptionGroup -- at the failure, not at the end of the program
    #     the BODY raises     the children are cancelled too, and the body's own error comes out
    #                         inside the group like a child's would
    #     several fail        every failure is carried; the group is not the first one wearing a
    #                         plural name

    def __init__(self):
        self._entered = False
        self._exiting = False
        self._aborting = False
        self._parent_task = None
        # Whether the cancellation the parent is about to see is OURS. Without this the block would
        # report a CancelledError it caused itself, hiding the child failure that caused it -- and
        # a cancellation from OUTSIDE would be indistinguishable and get swallowed.
        self._parent_cancel_requested = False
        self._loop = None
        self._tasks = []
        self._errors = []
        self._on_completed = None

    def __repr__(self):
        if not self._entered:
            return "<TaskGroup>"
        if self._aborting:
            return "<TaskGroup cancelling>"
        return "<TaskGroup entered>"

    async def __aenter__(self):
        if self._entered:
            raise RuntimeError("TaskGroup " + repr(self) + " has already been entered")
        self._loop = get_running_loop()
        self._parent_task = self._loop._current_task
        if self._parent_task is None:
            # Nothing to cancel and nothing to carry a failure out through, so the block could not
            # keep either of its promises. Saying so beats a group that quietly never forms.
            raise RuntimeError("TaskGroup " + repr(self) + " must be used inside a task")
        self._entered = True
        return self

    def create_task(self, coro):
        # A child of THIS block. Same task as `asyncio.create_task` makes -- awaitable, cancellable,
        # and its `result()` readable after the block -- with the group registered to be told when it
        # settles.
        if not self._entered:
            raise RuntimeError("TaskGroup " + repr(self) + " has not been entered")
        if self._exiting and not self._tasks:
            raise RuntimeError("TaskGroup " + repr(self) + " is finished")
        task = create_task(coro)
        task.add_done_callback(self._on_task_done)
        self._tasks.append(task)
        if self._aborting:
            # Created after a sibling already failed. Cancelling it here rather than letting it run is
            # what keeps "a failure stops the block" true for work started in a `finally`.
            task.cancel()
        return task

    def _abort(self):
        self._aborting = True
        for task in self._tasks:
            if not task.done():
                task.cancel()

    def _on_task_done(self, task):
        index = 0
        while index < len(self._tasks):
            if self._tasks[index] is task:
                del self._tasks[index]
                break
            index = index + 1
        # The waiter in `__aexit__` sleeps on a future rather than polling, so the last child to
        # finish is the one that wakes it.
        if self._on_completed is not None and not self._tasks:
            if not self._on_completed.done():
                self._on_completed.set_result(True)
        if task.cancelled():
            # A cancelled child is this group doing its job, not a failure to report. Reporting it
            # would put a CancelledError in the group beside the error that CAUSED the cancellation,
            # and the cause is the thing anybody wants to read.
            return
        error = task.exception()
        if error is None:
            return
        self._errors.append(error)
        if not self._aborting:
            self._abort()
            # >>> AND WAKE THE PARENT. Without this the block ends only when the last child does --
            # so a body sitting in a long `await` would not learn of the failure until whatever it is
            # waiting for finishes, which on a device is the never-finishing case all over again. The
            # cancellation is thrown into the parent wherever it is suspended, and `__aexit__` below
            # recognizes it as its own and replaces it with the group. <<<
            self._parent_cancel_requested = True
            self._parent_task.cancel()

    async def __aexit__(self, exc_type, exc, tb):
        self._exiting = True
        if exc is not None:
            if self._parent_cancel_requested and isinstance(exc, CancelledError):
                # The cancellation this group caused, arriving where the body was suspended. It is not
                # an error of the program's -- the group raised below carries the real one.
                pass
            else:
                # The BODY's own failure joins the children's, and cancels them: a block whose body
                # died has no reason to keep the work it started running.
                self._errors.append(exc)
                self._abort()
        # >>> THE WAIT THAT MAKES IT A BLOCK. Nothing is awaited explicitly by the program, and yet
        # the block cannot be left while a child is running. <<<
        while self._tasks:
            self._on_completed = self._loop.create_future()
            try:
                await self._on_completed
            except CancelledError:
                # A cancellation arriving while WAITING -- ours for a late failure, or the caller's.
                # Either way the children are cancelled and the wait resumes: leaving now would leave
                # them running, which is the one thing this block promises not to do.
                if not self._aborting:
                    self._abort()
            self._on_completed = None
        if self._errors:
            errors = self._errors
            self._errors = []
            # `BaseExceptionGroup` and not `ExceptionGroup`: the constructor NARROWS to the latter
            # when every error is an ordinary `Exception`, and keeps the wide class when one is not --
            # so a `SystemExit` among the children cannot end up inside something `except Exception:`
            # would take.
            raise BaseExceptionGroup("unhandled errors in a TaskGroup", errors)
        # Suppress only the cancellation this group caused and then found nothing to report for.
        return exc is not None and self._parent_cancel_requested and isinstance(exc, CancelledError)


class Condition:
    # A LOCK plus a waiting room: hold the lock, find the world not ready, and hand the lock back
    # while you wait for somebody to say it changed.
    #
    # WHY IT IS NOT AN `Event`. An Event is one flag with no ownership, so a waiter wakes and reads
    # state that another waiter may already have consumed. A Condition wakes you HOLDING THE LOCK, so
    # what you then read is what the notifier left -- which is the whole reason the predicate loop in
    # `wait_for` is correct rather than hopeful.

    def __init__(self, lock=None):
        self._lock = lock if lock is not None else Lock()
        self._waiters = []

    def locked(self):
        return self._lock.locked()

    async def acquire(self):
        return await self._lock.acquire()

    def release(self):
        self._lock.release()

    async def __aenter__(self):
        await self.acquire()
        return self

    async def __aexit__(self, exc_type, exc, tb):
        self.release()
        return False

    async def wait(self):
        # >>> THE LOCK IS RELEASED FOR THE WAIT AND RE-ACQUIRED BEFORE RETURNING, and both halves are
        # load-bearing. Holding it across the wait deadlocks against the notifier, which must hold the
        # lock to notify; returning without it leaves the caller's `async with` releasing a lock it
        # does not hold. <<<
        if not self.locked():
            raise RuntimeError("cannot wait on un-acquired lock")
        self.release()
        try:
            future = get_event_loop().create_future()
            self._waiters.append(future)
            try:
                await future
                return True
            finally:
                if future in self._waiters:
                    self._waiters.remove(future)
        finally:
            # Re-acquired even when the wait was CANCELLED. A cancellation still unwinds through the
            # caller's `async with`, which releases -- so skipping this to exit sooner hands the lock
            # away twice.
            await self.acquire()

    async def wait_for(self, predicate):
        # A LOOP, and for the reason `Queue.get` loops: being notified means the predicate was true
        # for somebody, not that it is still true for you.
        result = predicate()
        while not result:
            await self.wait()
            result = predicate()
        return result

    def notify(self, n=1):
        if not self.locked():
            raise RuntimeError("cannot notify on un-acquired lock")
        woken = 0
        for future in self._waiters:
            if woken >= n:
                break
            if not future.done():
                future.set_result(True)
                woken += 1

    def notify_all(self):
        self.notify(len(self._waiters))


class BrokenBarrierError(RuntimeError):
    pass


class Barrier:
    # A rendezvous for a FIXED number of coroutines: everybody waits until the last one arrives, and
    # then they all go. Reusable -- the count resets as the party is released, so a loop can barrier
    # once per iteration.

    def __init__(self, parties):
        if parties < 1:
            raise ValueError("parties must be > 0")
        self._parties = parties
        self._count = 0
        self._waiters = []
        self._broken = False

    @property
    def parties(self):
        return self._parties

    @property
    def n_waiting(self):
        return self._count

    @property
    def broken(self):
        return self._broken

    async def wait(self):
        if self._broken:
            raise BrokenBarrierError()
        index = self._count
        self._count += 1
        if self._count >= self._parties:
            # The last to arrive releases everybody and resets the count IN THE SAME STEP, so a
            # waiter woken here can call `wait()` again and start the next generation without
            # observing a half-reset barrier.
            self._count = 0
            waiting = self._waiters
            self._waiters = []
            for future in waiting:
                if not future.done():
                    future.set_result(True)
            return index
        future = get_event_loop().create_future()
        self._waiters.append(future)
        await future
        if self._broken:
            raise BrokenBarrierError()
        return index

    def _release_broken(self):
        self._broken = True
        self._count = 0
        waiting = self._waiters
        self._waiters = []
        for future in waiting:
            if not future.done():
                future.set_exception(BrokenBarrierError())

    def abort(self):
        # Breaks it PERMANENTLY: everyone waiting, and everyone who arrives later, gets a
        # BrokenBarrierError. For the case where a party has failed and the rendezvous can never
        # complete -- the alternative is every other party waiting forever.
        self._release_broken()

    def reset(self):
        # Returns an unused barrier to its initial state. Anyone already waiting is broken out,
        # because they were waiting for a generation that will now never fill.
        if self._waiters:
            self._release_broken()
        self._broken = False
        self._count = 0


class LifoQueue(Queue):
    # Last in, first out -- a stack with a queue's waiting. Everything but the end an item leaves
    # from is inherited.

    def _get(self):
        return self._items.pop()


# The binary heap `PriorityQueue` orders by: `heapq`'s own two operations and its own sift
# algorithm, kept here rather than imported.
#
# Importing a module builds every function in it as a heap object, and a priority queue needs two of
# the sixteen `heapq` defines -- so the two are here and the import is not.
#
# This is `heapq`'s algorithm rather than a rewrite of it, and the distinction matters: a heap's
# order among EQUAL entries is a property of the sift and of nothing else, so a heap that merely
# sorts correctly is still a different data structure. Parents move down until the new item fits,
# and a pop bubbles the smaller child up before sifting the last element back down.
def _heap_siftdown(heap, startpos, pos):
    newitem = heap[pos]
    while pos > startpos:
        parentpos = (pos - 1) >> 1
        parent = heap[parentpos]
        if newitem < parent:
            heap[pos] = parent
            pos = parentpos
            continue
        break
    heap[pos] = newitem


def _heap_siftup(heap, pos):
    endpos = len(heap)
    startpos = pos
    newitem = heap[pos]
    childpos = 2 * pos + 1
    while childpos < endpos:
        rightpos = childpos + 1
        if rightpos < endpos and not heap[childpos] < heap[rightpos]:
            childpos = rightpos
        heap[pos] = heap[childpos]
        pos = childpos
        childpos = 2 * pos + 1
    heap[pos] = newitem
    _heap_siftdown(heap, startpos, pos)


class PriorityQueue(Queue):
    # Lowest first, by the items' own ordering. Entries are usually `(priority, payload)` tuples,
    # which is what makes ties break on the payload rather than on arrival.

    def _put(self, item):
        self._items.append(item)
        _heap_siftdown(self._items, 0, len(self._items) - 1)

    def _get(self):
        last = self._items.pop()
        if self._items:
            smallest = self._items[0]
            self._items[0] = last
            _heap_siftup(self._items, 0)
            return smallest
        return last


# What `wait()` is being asked to wait FOR. Strings, exactly as in CPython, so a program that prints
# one gets the name and not an opaque number.
FIRST_COMPLETED = "FIRST_COMPLETED"
FIRST_EXCEPTION = "FIRST_EXCEPTION"
ALL_COMPLETED = "ALL_COMPLETED"


async def wait(fs, timeout=None, return_when=ALL_COMPLETED):
    # Waits on a group and reports `(done, pending)` -- WITHOUT raising what any of them raised. That
    # is the difference from `gather`, and it is why this is the primitive a supervisor wants: a
    # failure is something to inspect in `done`, not something that unwinds the waiter.
    if not fs:
        raise ValueError("Set of Tasks/Futures is empty.")
    if return_when not in (FIRST_COMPLETED, FIRST_EXCEPTION, ALL_COMPLETED):
        raise ValueError("Invalid return_when value: " + str(return_when))
    futures = []
    for f in fs:
        if iscoroutine(f):
            # CPython 3.12 and later refuse this rather than wrapping it, because a coroutine passed
            # here would be wrapped in a task the CALLER cannot then cancel or read.
            raise TypeError("Passing coroutines is forbidden, use tasks explicitly.")
        futures.append(f)
    loop = get_event_loop()
    waiter = loop.create_future()
    outstanding = [len(futures)]
    handle = [None]

    def _release():
        if not waiter.done():
            waiter.set_result(None)

    def _one_done(f):
        outstanding[0] -= 1
        if outstanding[0] <= 0 or return_when == FIRST_COMPLETED:
            _release()
            return
        if return_when == FIRST_EXCEPTION and not f.cancelled() and f.exception() is not None:
            _release()

    if timeout is not None:
        handle[0] = loop.call_later(timeout, _release)
    for f in futures:
        f.add_done_callback(_one_done)
    try:
        await waiter
    finally:
        if handle[0] is not None:
            handle[0].cancel()
        # UNREGISTERED, always. The futures still pending outlive this call, and a callback left on
        # one of them keeps this frame's closure alive with it.
        for f in futures:
            f.remove_done_callback(_one_done)
    done = set()
    pending = set()
    for f in futures:
        if f.done():
            done.add(f)
        else:
            pending.add(f)
    return done, pending


def as_completed(fs, timeout=None):
    # Iterates the group in COMPLETION order: each item is an awaitable that produces the next result
    # to arrive, whichever future that turns out to be. What it gives a caller that `wait` does not is
    # the ability to act on the first answer before the slowest has finished.
    todo = []
    for f in fs:
        todo.append(ensure_future(f))
    loop = get_event_loop()
    finished = []  # completed futures, in the order they completed
    waiters = []   # futures handed out by `_next_done`, each owed a completion
    handle = [None]

    def _hand_over():
        while finished and waiters:
            waiter = waiters.pop(0)
            if not waiter.done():
                waiter.set_result(finished.pop(0))

    def _one_done(f):
        if f in todo:
            todo.remove(f)
        finished.append(f)
        _hand_over()
        if not todo and handle[0] is not None:
            handle[0].cancel()
            handle[0] = None

    def _on_timeout():
        # Everything still outstanding is abandoned rather than cancelled -- CPython leaves the
        # futures running and only stops REPORTING them, because the caller still holds them and may
        # want them.
        for f in todo:
            f.remove_done_callback(_one_done)
        while waiters:
            waiter = waiters.pop(0)
            if not waiter.done():
                waiter.set_exception(TimeoutError())

    async def _next_done():
        if finished:
            return finished.pop(0).result()
        waiter = loop.create_future()
        waiters.append(waiter)
        completed = await waiter
        return completed.result()

    if timeout is not None:
        handle[0] = loop.call_later(timeout, _on_timeout)
    for f in todo:
        f.add_done_callback(_one_done)
    results = []
    for _ in range(len(todo) + len(finished)):
        results.append(_next_done())
    return results


def shield(aw):
    # Awaits `aw` while HIDING the caller's cancellation from it: cancelling what this returns leaves
    # the inner operation running.
    #
    # For the case where the work must finish even though this caller has stopped waiting -- a write
    # that has already been half-committed, a lock that must be released by the same task that took
    # it. The cost is the obvious one, and it is CPython's too: the inner task then has nobody
    # waiting on it, so an exception it raises is nobody's until the loop reports it.
    inner = ensure_future(aw)
    if inner.done():
        return inner
    outer = get_event_loop().create_future()

    def _inner_done(f):
        if outer.cancelled():
            if not f.cancelled():
                f.exception()  # retrieved, so the loop does not report a failure that was shielded
            return
        if f.cancelled():
            outer.cancel()
            return
        error = f.exception()
        if error is not None:
            outer.set_exception(error)
        else:
            outer.set_result(f.result())

    inner.add_done_callback(_inner_done)
    return outer
