# lamella.clock, bundled as a MANAGED module over the native clock seam.
#
# `is_set()` answers whether an embedder anchored this runtime's wall clock. The same concept ships
# in all three languages with one spelling each -- `Lamella.Runtime.Clock.IsSet()` in C# and
# `Lamella.Clock.isSet()` in ECMAScript -- so only the prefix and the casing differ.
#
# IT IS A FUNCTION, NOT A PROPERTY. The answer changes the moment something anchors the clock, and a
# property reads like a snapshot a caller may cache.
#
# IT CLAIMS PROVENANCE, NOT QUALITY. It says somebody anchored this clock. It does not say the
# reading is right: an anchored clock may have been stepped without anything noticing, is no more
# accurate than the monotonic source beneath it, and loses the whole duration of a deep sleep. A name
# promising validity, accuracy or synchronization would promise what this cannot deliver.
#
# WHY IT LIVES HERE AND NOT ON `time`: `time` is CPython's module and this is not CPython's function.
# The convention is that anything of ours goes under `lamella.*`, members included, and nothing is
# invented inside a namespace we do not own.
import _time


def is_set():
    # True once an embedder has installed a wall clock. False means `time.time()` is counting from
    # the epoch rather than from an anchored date -- a reading a caller can recognize as 1970, which
    # is why that call answers instead of refusing.
    return _time.clock_is_set()
