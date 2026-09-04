# threading -- Thread, Lock, RLock, Event, written over the `_thread` green-thread seam.
#
# WHAT A THREAD IS HERE. A green thread: a suspended call stack that the interpreter's scheduler
# rotates on a 256-op budget. It is PREEMPTIVE -- a worker that never sleeps, locks or yields still
# takes its turn -- so a `while True:` worker written by a beginner works on a microcontroller. What
# it is not is an OS thread: there is one object heap, one interpreter, and no parallelism.
#
# THE CAP IS FOUR, and it is a property of the family rather than of this tier. The compiled tier's
# threads carry preallocated native stacks at a fixed address, so four is a hard limit there -- and
# an interpreter that accepted a fifth would run programs the deployed image refuses, which is the
# preview inverting in the direction nobody checks. `Thread.start` past the cap raises RuntimeError.
#
# WHY THESE LOCKS ARE NOT asyncio's. `asyncio.Lock` never blocks a thread: it suspends a coroutine
# and lets the loop keep running. A lock here BLOCKS the green thread that asked for it, and only a
# release wakes it. Building one out of the other gives Python a lock that deadlocks an event loop,
# so they are two primitives that happen to share a name.

import _thread


def get_ident():
    # The running thread's id. `0` is the main thread and is never handed to a spawned one.
    return _thread.get_ident()


def active_count():
    # Threads that have not finished, counting the caller. One asked for during THIS slice counts
    # too: it has been accepted against the cap, so reporting it later would mean the count and the
    # cap disagreed about the same program.
    return _thread.active_count()


class Lock:
    # Mutual exclusion between green threads. FIFO, and the hand-off is what makes that mean
    # something: `release` gives the lock DIRECTLY to the longest-waiting thread rather than
    # unlocking and waking it to try again. Unlocking first leaves a window in which a thread that
    # never queued takes the lock ahead of one that has waited, and then two believe they hold it.
    #
    # `_cell` is a one-element list holding `owner_id + 1`, or `0` when free. It is a list because
    # the seam has to TEST AND SET it with no thread switch in between, and only a native call is
    # indivisible here -- `if not self._locked: self._locked = True` is two ops with a switch point
    # between them, and two threads take the same lock.

    def __init__(self):
        self._cell = [0]
        self._waiters = []

    def acquire(self, blocking=True, timeout=-1):
        if timeout != -1:
            raise NotImplementedError(
                "a timeout needs a deadline the thread scheduler can park on, which this runtime "
                "does not have; use acquire(blocking=False) and retry, or an asyncio.Lock"
            )
        return _thread.acquire_lock(self._cell, self._waiters, blocking)

    def release(self):
        _thread.release_lock(self._cell, self._waiters)

    def locked(self):
        return self._cell[0] != 0

    def __enter__(self):
        self.acquire()
        return True

    def __exit__(self, exc_type, exc_value, traceback):
        self.release()
        return False


class RLock:
    # The same lock, re-entrant: the thread holding it may take it again and releases it as many
    # times as it took it. The recursion count is ordinary Python state because only the OWNER ever
    # touches it -- no other thread can be inside these branches while the lock is held.

    def __init__(self):
        self._cell = [0]
        self._waiters = []
        self._count = 0

    def acquire(self, blocking=True, timeout=-1):
        if timeout != -1:
            raise NotImplementedError(
                "a timeout needs a deadline the thread scheduler can park on, which this runtime "
                "does not have; use acquire(blocking=False) and retry, or an asyncio.Lock"
            )
        if self._cell[0] == _thread.get_ident() + 1:
            self._count = self._count + 1
            return True
        if not _thread.acquire_lock(self._cell, self._waiters, blocking):
            return False
        self._count = 1
        return True

    def release(self):
        if self._cell[0] != _thread.get_ident() + 1:
            raise RuntimeError("cannot release un-acquired lock")
        self._count = self._count - 1
        if self._count == 0:
            _thread.release_lock(self._cell, self._waiters)

    def __enter__(self):
        self.acquire()
        return True

    def __exit__(self, exc_type, exc_value, traceback):
        self.release()
        return False


class Event:
    # A flag threads can wait for. `set` wakes everyone waiting and stays set until `clear`.
    #
    # The flag lives in the same kind of one-element list a Lock uses, for a sharper reason: the TEST
    # and the PARK have to be one indivisible step. Written as `if not self._flag: park()` the flag
    # can be set between the two ops -- the setter wakes an empty queue, the waiter then parks, and
    # nothing will ever wake it. That is a lost wakeup, and it is rare enough to reach a device.

    def __init__(self):
        self._cell = [0]
        self._waiters = []

    def is_set(self):
        return self._cell[0] != 0

    def set(self):
        self._cell[0] = 1
        _thread.wake_all(self._waiters)

    def clear(self):
        self._cell[0] = 0

    def wait(self, timeout=None):
        if timeout is not None:
            raise NotImplementedError(
                "a timeout needs a deadline the thread scheduler can park on, which this runtime "
                "does not have; wait() without one, or use an asyncio.Event"
            )
        # Re-tested after a wake because `clear` may have run in between: CPython's `Event.wait`
        # reports the flag as it is when the waiter RUNS, not as it was when it woke.
        while self._cell[0] == 0:
            _thread.wait_flag(self._cell, self._waiters)
        return True


def _bootstrap(thread):
    # >>> A MODULE-LEVEL FUNCTION AND NOT A METHOD, BECAUSE THE SEAM TAKES A PLAIN FUNCTION. A
    # thread's target has to be something the scheduler can build a frame for on somebody's behalf,
    # and a bound method is not: it needs a call the scheduler cannot make. The Thread rides in as an
    # ARGUMENT instead, which costs nothing and leaves `target=obj.method` working -- that one is
    # called by `run`, which is ordinary Python on the thread's own stack. <<<
    try:
        thread.run()
    finally:
        thread._done[0] = 1


class Thread:
    # `Thread(target=fn, args=(...))`, or a subclass overriding `run`.

    def __init__(self, group=None, target=None, name=None, args=(), kwargs=None, daemon=None):
        if group is not None:
            raise ValueError("group argument must be None for now")
        if kwargs:
            raise NotImplementedError(
                "keyword arguments to a thread target are not supported; bind them with a closure"
            )
        self._target = target
        self._args = tuple(args)
        self._ident = None
        self._started = False
        # A one-element list so the last act of the thread can mark it finished with no runtime hook:
        # `is_alive` reads exactly what `_bootstrap` wrote.
        self._done = [0]
        self._name = name
        self.daemon = bool(daemon)

    def start(self):
        if self._started:
            raise RuntimeError("threads can only be started once")
        # The cap is enforced at the seam and raises RuntimeError there, so a Thread that fails to
        # start is NOT marked started and can be started again once room appears.
        ident = _thread.start_new_thread(_bootstrap, (self,))
        self._started = True
        self._ident = ident
        if self._name is None:
            self._name = "Thread-" + str(ident)
        _active[ident] = self

    def run(self):
        if self._target is not None:
            self._target(*self._args)

    def join(self, timeout=None):
        if timeout is not None:
            raise NotImplementedError(
                "a timeout needs a deadline the thread scheduler can park on, which this runtime "
                "does not have; join() without one"
            )
        if not self._started:
            raise RuntimeError("cannot join thread before it is started")
        if self._ident == _thread.get_ident():
            raise RuntimeError("cannot join current thread")
        _thread.join(self._ident)

    def is_alive(self):
        return self._started and self._done[0] == 0

    @property
    def ident(self):
        return self._ident

    @property
    def name(self):
        return self._name

    @name.setter
    def name(self, value):
        self._name = str(value)


class _MainThread(Thread):
    # The thread the program started on. It has no target and was never `start`ed, so its state is
    # written here instead: it is running by definition, which is why `current_thread()` answers
    # before any Thread object exists.

    def __init__(self):
        Thread.__init__(self, name="MainThread")
        self._started = True
        self._ident = 0


_main_thread = _MainThread()
_active = {0: _main_thread}


def main_thread():
    return _main_thread


def current_thread():
    # A thread started through `_thread.start_new_thread` directly, rather than through this module,
    # has no Thread object. CPython synthesizes one; this answers the main thread, which is the
    # honest approximation -- there is nothing here that describes that thread.
    return _active.get(_thread.get_ident(), _main_thread)
