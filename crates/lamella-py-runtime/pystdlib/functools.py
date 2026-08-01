# The pure-Python half of the standard library: functools, bundled as a MANAGED module.
#
# Unlike collections (whose types subclass dict / generate classes, so they are NATIVE), these
# are plain functions and closures -- exactly what the managed-module machinery exists for. The
# harnesses' bundle resolver serves this directory, so `from functools import reduce` compiles
# the module into the program's bundle and the differential verifies it against CPython.
#
# Subset notes: lru_cache caches POSITIONAL argument tuples (keyword-argument callers bypass
# design intent, as documented); wraps copies the __wrapped__ backlink (the metadata CPython
# copies -- __name__/__doc__ -- rides function attributes where the runtime exposes them).


def reduce(function, iterable, *start):
    if len(start) > 1:
        raise TypeError("reduce expected at most 3 arguments, got " + str(2 + len(start)))
    it = iter(iterable)
    if start:
        value = start[0]
    else:
        ok = True
        try:
            value = next(it)
        except StopIteration:
            ok = False
        if not ok:
            raise TypeError("reduce() of empty iterable with no initial value")
    for element in it:
        value = function(value, element)
    return value


def wraps(wrapped):
    def decorator(wrapper):
        try:
            wrapper.__wrapped__ = wrapped
        except AttributeError:
            pass
        return wrapper
    return decorator


def partial(func, *args):
    def inner(*more):
        full = list(args) + list(more)
        return func(*full)
    return inner


def lru_cache(maxsize=128):
    # Bare `@lru_cache` (no call) receives the function directly.
    if callable(maxsize):
        return lru_cache(128)(maxsize)

    def decorator(function):
        from collections import OrderedDict
        cache = OrderedDict()

        def wrapper(*args):
            key = args
            if key in cache:
                cache.move_to_end(key)
                return cache[key]
            value = function(*args)
            cache[key] = value
            if maxsize is not None and len(cache) > maxsize:
                cache.popitem(False)
            return value

        wrapper.__wrapped__ = function
        return wrapper

    return decorator


# total_ordering: fill in the missing rich-comparison methods from one the class defines plus __eq__.
# Each helper computes its operator from the root via `type(self).__root__(self, other)`, propagating
# NotImplemented so a foreign operand still defers to the reflected op (CPython's exact logic). Root
# detection uses `op in cls.__dict__` -- the class's OWN namespace -- rather than CPython's
# `getattr(cls, op) is not getattr(object, op)`; identical for the common case (the comparison is
# defined on the decorated class), and the interpreter exposes no bare `object` to compare against.
def _gt_from_lt(self, other):
    op_result = type(self).__lt__(self, other)
    if op_result is NotImplemented:
        return op_result
    return not op_result and self != other


def _le_from_lt(self, other):
    op_result = type(self).__lt__(self, other)
    if op_result is NotImplemented:
        return op_result
    return op_result or self == other


def _ge_from_lt(self, other):
    op_result = type(self).__lt__(self, other)
    if op_result is NotImplemented:
        return op_result
    return not op_result


def _ge_from_le(self, other):
    op_result = type(self).__le__(self, other)
    if op_result is NotImplemented:
        return op_result
    return not op_result or self == other


def _lt_from_le(self, other):
    op_result = type(self).__le__(self, other)
    if op_result is NotImplemented:
        return op_result
    return op_result and self != other


def _gt_from_le(self, other):
    op_result = type(self).__le__(self, other)
    if op_result is NotImplemented:
        return op_result
    return not op_result


def _lt_from_gt(self, other):
    op_result = type(self).__gt__(self, other)
    if op_result is NotImplemented:
        return op_result
    return not op_result and self != other


def _ge_from_gt(self, other):
    op_result = type(self).__gt__(self, other)
    if op_result is NotImplemented:
        return op_result
    return not op_result or self == other


def _le_from_gt(self, other):
    op_result = type(self).__gt__(self, other)
    if op_result is NotImplemented:
        return op_result
    return not op_result


def _le_from_ge(self, other):
    op_result = type(self).__ge__(self, other)
    if op_result is NotImplemented:
        return op_result
    return not op_result or self == other


def _gt_from_ge(self, other):
    op_result = type(self).__ge__(self, other)
    if op_result is NotImplemented:
        return op_result
    return op_result and self != other


def _lt_from_ge(self, other):
    op_result = type(self).__ge__(self, other)
    if op_result is NotImplemented:
        return op_result
    return not op_result


_convert = {
    "__lt__": [("__gt__", _gt_from_lt), ("__le__", _le_from_lt), ("__ge__", _ge_from_lt)],
    "__le__": [("__ge__", _ge_from_le), ("__lt__", _lt_from_le), ("__gt__", _gt_from_le)],
    "__gt__": [("__lt__", _lt_from_gt), ("__ge__", _ge_from_gt), ("__le__", _le_from_gt)],
    "__ge__": [("__le__", _le_from_ge), ("__gt__", _gt_from_ge), ("__lt__", _lt_from_ge)],
}


def total_ordering(cls):
    roots = {op for op in _convert if op in cls.__dict__}
    if not roots:
        raise ValueError("must define at least one ordering operation: < > <= >=")
    root = max(roots)  # prefer __lt__ > __le__ > __gt__ > __ge__ (lexical max of the four names)
    for opname, opfunc in _convert[root]:
        if opname not in roots:
            opfunc.__name__ = opname
            setattr(cls, opname, opfunc)
    return cls
