"""Shallow and deep copies of objects.

`copy(x)` reproduces the object itself and keeps sharing whatever it refers to; `deepcopy(x)`
reproduces what it refers to as well, however deeply nested, and terminates on a structure that
refers back to itself.

An object can customize copying with `__copy__` or `__deepcopy__` (Python 3.14 Library Reference,
copy). This module rebuilds other class instances through `__new__` and the state protocol
described by the Python 3.14 Library Reference, pickle, `object.__getstate__` and
`object.__setstate__`. Exceptions are rebuilt by calling their class with stored constructor
arguments before restoring state; those arguments are described by the Python 3.14 Library
Reference, Built-in Exceptions, BaseException.args.
"""


class Error(Exception):
    """Raised when an object cannot be copied."""


error = Error

# Immutable and holding nothing a copy could differ in: the object itself IS its own copy. A tuple
# belongs here for a SHALLOW copy only -- it cannot change, but what it holds can.
_SHALLOW_ATOMIC = (int, float, bool, str, bytes, tuple, frozenset, range, slice, type)
_DEEP_ATOMIC = (int, float, bool, str, bytes, range, slice, type)


def _cannot(x):
    """The refusal for an object with no way to be reproduced."""
    if isinstance(x, memoryview):
        return TypeError("cannot pickle memoryview objects")
    return TypeError("cannot pickle '" + type(x).__name__ + "' object")


def _rebuild(x, memo):
    """A fresh instance of `x`'s class carrying `x`'s state -- deeply when `memo` is not None.

    Ordinary instances use the state protocol (Python 3.14 Library Reference, pickle,
    `object.__getstate__` and `object.__setstate__`). Exceptions instead supply stored args to
    their constructor (Python 3.14 Library Reference, Built-in Exceptions, BaseException.args).
    """
    for name in ("__reduce_ex__", "__reduce__", "__getnewargs_ex__", "__getnewargs__"):
        if getattr(x, name, None) is not None:
            raise Error(
                "copying an object through " + name + " is not supported; define __copy__ or "
                "__deepcopy__ on " + type(x).__name__ + " instead"
            )
    cls = type(x)
    if isinstance(x, BaseException):
        # Use the stored constructor arguments, even when a subclass shadows args. The constructor
        # supplies the new exception's code, then the instance dictionary is restored below.
        # Python 3.14 Library Reference, pickle, object.__reduce__, describes reconstruction from
        # a callable, its arguments and state; Built-in Exceptions, BaseException.args, names args.
        args = BaseException.args.__get__(x)
        if memo is not None:
            args = deepcopy(args, memo)
        made = cls(*args)
    else:
        made = cls.__new__(cls)
    if memo is not None:
        memo[id(x)] = made
    state = x.__getstate__()
    if state is None:
        return made
    if memo is not None:
        state = deepcopy(state, memo)
    restore = getattr(made, "__setstate__", None)
    if restore is not None:
        restore(state)
        return made
    slot_state = None
    if isinstance(state, tuple) and len(state) == 2:
        state, slot_state = state
    if state is not None:
        made.__dict__.update(state)
    if slot_state is not None:
        for name in slot_state:
            setattr(made, name, slot_state[name])
    return made


def copy(x):
    """A shallow copy of `x`: a new object holding the same references."""
    if x is None or x is Ellipsis or x is NotImplemented:
        return x
    if isinstance(x, _SHALLOW_ATOMIC):
        return x
    hook = getattr(x, "__copy__", None)
    if hook is not None:
        return hook()
    # Every mutable built-in container reproduces itself, which keeps a subtype a subtype -- a
    # defaultdict copies to a defaultdict with the same factory, a deque keeps its maxlen.
    if _has_copy_method(x):
        return x.copy()
    if getattr(x, "__getstate__", None) is not None:
        return _rebuild(x, None)
    if callable(x) or _by_reference(x):
        # A function, a built-in, a bound method or a member descriptor: named, not built, so a copy
        # of one is itself.
        return x
    raise _cannot(x)


def deepcopy(x, memo=None):
    """A deep copy of `x`: `x` and everything reachable from it, reproduced.

    `memo` maps the identity of an already-copied object to its copy. It is what makes a structure
    that refers back to itself terminate, and what keeps two references to one object inside `x`
    two references to ONE copy.
    """
    if x is None or x is Ellipsis or x is NotImplemented:
        return x
    if isinstance(x, _DEEP_ATOMIC):
        return x
    if memo is None:
        memo = {}
    key = id(x)
    if key in memo:
        return memo[key]
    hook = getattr(x, "__deepcopy__", None)
    if hook is not None:
        made = hook(memo)
        memo[key] = made
        return made
    if isinstance(x, tuple):
        # A tuple cannot change, so it only needs reproducing if one of its elements did: a tuple of
        # atomics deep-copies to itself, and `deepcopy((a, b)) is (a, b)` stays true for those.
        parts = []
        changed = False
        for value in x:
            part = deepcopy(value, memo)
            changed = changed or part is not value
            parts.append(part)
        made = tuple(parts) if changed else x
        memo[key] = made
        return made
    if isinstance(x, list):
        made = []
        memo[key] = made
        for value in x:
            made.append(deepcopy(value, memo))
        return made
    if isinstance(x, dict):
        # Emptying a copy keeps the subtype and its configuration (a defaultdict's factory) without
        # naming either -- there is no portable way to reconstruct one from the outside.
        made = x.copy()
        made.clear()
        memo[key] = made
        for k in x:
            made[deepcopy(k, memo)] = deepcopy(x[k], memo)
        return made
    if isinstance(x, frozenset):
        parts = []
        for value in x:
            parts.append(deepcopy(value, memo))
        made = frozenset(parts)
        memo[key] = made
        return made
    if isinstance(x, set):
        made = x.copy()
        made.clear()
        memo[key] = made
        for value in x:
            made.add(deepcopy(value, memo))
        return made
    if isinstance(x, bytearray):
        made = bytearray(x)
        memo[key] = made
        return made
    if _has_copy_method(x):
        made = x.copy()
        made.clear()
        memo[key] = made
        for value in x:
            made.append(deepcopy(value, memo))
        return made
    if getattr(x, "__getstate__", None) is not None:
        return _rebuild(x, memo)
    if callable(x) or _by_reference(x):
        return x
    raise _cannot(x)


def _by_reference(x):
    """Whether `x` is a native member descriptor that this module copies by identity.

    The Python 3.14 Library Reference, copy, points to the pickle reduction protocol
    (object.__reduce__). This predicate selects the descriptors returned without rebuilding.
    """
    return type(x) is type(SystemExit.code)


def _has_copy_method(x):
    """Whether `x` is a built-in container that reproduces itself with `.copy()`."""
    if isinstance(x, (list, dict, set, bytearray)):
        return True
    if isinstance(x, str) or isinstance(x, bytes) or isinstance(x, frozenset):
        return False
    # A deque, and anything else built-in that grew the same method. An INSTANCE with a `copy`
    # attribute is excluded: its state protocol is the one that reproduces it faithfully.
    if getattr(x, "__getstate__", None) is not None:
        return False
    return getattr(x, "copy", None) is not None and getattr(x, "append", None) is not None
