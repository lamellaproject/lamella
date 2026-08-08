# time, bundled as a MANAGED module over the native clock seam.
#
# The seam is deliberately the smallest thing that must be native -- read the host's clock, in
# nanoseconds -- and everything a program actually calls is built here: seconds as floats, the sleep
# argument, and the aliases. A runtime whose embedder installed no clock says so when asked, rather
# than answering zero: a fabricated time is indistinguishable from a real one at the call site.
#
# `time()` is the WALL clock and can jump when the system clock is adjusted; `monotonic()` cannot go
# backwards but has an arbitrary origin, so only differences of it mean anything. `perf_counter()` is
# the same source as `monotonic()` here, which the standard permits.
#
# NOT PROVIDED: the calendar surface (`localtime`, `gmtime`, `mktime`, `strftime`, `struct_time`),
# which needs a timezone database and a civil-calendar conversion rather than a wrapper over a
# counter.
import _time

_NS_PER_SECOND = 1000000000


def time_ns():
    return _time.time_ns()


def monotonic_ns():
    return _time.monotonic_ns()


def perf_counter_ns():
    return _time.monotonic_ns()


def time():
    # Seconds since the epoch as a float. The division is the only lossy step, and it is the one
    # CPython's `time()` makes too -- `time_ns()` exists for callers who cannot afford it.
    return _time.time_ns() / _NS_PER_SECOND


def monotonic():
    return _time.monotonic_ns() / _NS_PER_SECOND


def perf_counter():
    return _time.monotonic_ns() / _NS_PER_SECOND


def sleep(seconds):
    # Accepts an int or a float, like CPython -- including bool, which IS an int. A negative sleep
    # is an error there and here, rather than a silent return.
    if not isinstance(seconds, (int, float)):
        raise TypeError(
            "'" + type(seconds).__name__ + "' object cannot be interpreted as an integer or float"
        )
    if seconds < 0:
        raise ValueError("sleep length must be non-negative")
    _time.sleep_ns(int(seconds * _NS_PER_SECOND))
