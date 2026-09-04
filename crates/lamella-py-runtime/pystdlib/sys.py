# sys, bundled as a MANAGED module: the interpreter's own facts about itself and the machine it is on.
#
# The values that are properties of the MACHINE come from the `_platform` seam, which derives them from
# the compiler that built the runtime -- so `byteorder` and `maxsize` are the same answers the C library
# on that target would give, and on a host they agree with the CPython running beside them.
#
# The values that are properties of THIS interpreter say what it is rather than impersonating another.
# `implementation.name` is "lamella"; `version_info` is the CPython language level this runtime tracks,
# which is the established convention for an alternative implementation and is a statement about which
# specification is being followed, not a claim of complete coverage.
#
# NOT PROVIDED -- absent rather than stubbed, so a missing attribute is visible at the call site:
# `stdout` / `stdin` (they want the read side of a file object), `modules` and `path` (the import
# machinery's own state), `getsizeof`, `settrace`, and the frame accessors.
import _platform
import _sys

# --- the machine ---
byteorder = _platform.byteorder
maxsize = _platform.maxsize

# --- this interpreter ---
platform = "lamella"


class _Implementation:
    # Named like CPython's `sys.implementation` so `sys.implementation.name` reads the same, and so a
    # program can tell which interpreter it is on without a version-string parse.
    def __init__(self, name, version):
        self.name = name
        self.version = version

    def __repr__(self):
        return "namespace(name=" + repr(self.name) + ", version=" + repr(self.version) + ")"


def _version_tuple(text):
    parts = []
    for piece in text.split("."):
        digits = ""
        for c in piece:
            if c.isdecimal():
                digits = digits + c
            else:
                break
        if digits == "":
            parts.append(0)
        else:
            parts.append(int(digits))
    while len(parts) < 3:
        parts.append(0)
    return (parts[0], parts[1], parts[2])


# The CPython language level this runtime is written against; every semantic here is grounded in that
# specification. It is not a claim that every corner of it is implemented.
version_info = (3, 14, 6, "final", 0)

implementation = _Implementation("lamella", _version_tuple(_platform.version))
version = "3.14.6; Lamella " + _platform.version

argv = []


def exit(status=None):
    # CPython raises SystemExit rather than stopping the interpreter where it stands, so a `finally`
    # still runs and a caller can catch it.
    if status is None:
        raise SystemExit()
    raise SystemExit(status)


# --- the diagnostic stream ---


class _StdErr:
    # `sys.stderr`, with the two methods a diagnostic writer actually calls.
    #
    # WHY IT IS A CLASS AND NOT THE SEAM FUNCTION ITSELF: `sys.stderr.write(...)` is what CPython
    # programs write, and `print(..., file=sys.stderr)` passes the OBJECT and calls `write` on it.
    # Exposing the raw function under the name would work for neither.
    #
    # It is not a file object: no `read`, no `close`, no `fileno`. A stream a program can only
    # write to is what this runtime has, and the absent methods are absent rather than raising a
    # stub's error, so `hasattr` tells the truth.

    def write(self, text):
        # CPython returns the number of CHARACTERS written, and code in the wild does use it.
        _sys.stderr_write(text)
        return len(text)

    def flush(self):
        # Nothing is held back: a write leaves through the seam immediately, so there is nothing to
        # flush. Present because callers flush after writing and an AttributeError there would
        # convert a diagnostic into a second failure.
        pass


#: The error stream. Diagnostics, tracebacks, and the event loop's report of a task that died with
#: nobody to tell it.
stderr = _StdErr()
