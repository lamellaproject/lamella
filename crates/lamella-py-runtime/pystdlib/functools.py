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


# total_ordering. Each method it adds calls the class's own ordering method, looked up on the type
# rather than the instance, returns NotImplemented unchanged so that Python tries the reflected
# operation, and otherwise combines the result with == or !=. The twelve are shared by every class
# they are added to.
def _gt_from_lt(self, other):
    result = type(self).__lt__(self, other)
    return result if result is NotImplemented else not result and self != other


def _le_from_lt(self, other):
    result = type(self).__lt__(self, other)
    return result if result is NotImplemented else result or self == other


def _ge_from_lt(self, other):
    result = type(self).__lt__(self, other)
    return result if result is NotImplemented else not result


def _ge_from_le(self, other):
    result = type(self).__le__(self, other)
    return result if result is NotImplemented else not result or self == other


def _lt_from_le(self, other):
    result = type(self).__le__(self, other)
    return result if result is NotImplemented else result and self != other


def _gt_from_le(self, other):
    result = type(self).__le__(self, other)
    return result if result is NotImplemented else not result


def _lt_from_gt(self, other):
    result = type(self).__gt__(self, other)
    return result if result is NotImplemented else not result and self != other


def _ge_from_gt(self, other):
    result = type(self).__gt__(self, other)
    return result if result is NotImplemented else result or self == other


def _le_from_gt(self, other):
    result = type(self).__gt__(self, other)
    return result if result is NotImplemented else not result


def _le_from_ge(self, other):
    result = type(self).__ge__(self, other)
    return result if result is NotImplemented else not result or self == other


def _gt_from_ge(self, other):
    result = type(self).__ge__(self, other)
    return result if result is NotImplemented else result and self != other


def _lt_from_ge(self, other):
    result = type(self).__ge__(self, other)
    return result if result is NotImplemented else not result


# The four ordering methods, most preferred first, each with the three built from it in the order
# they are added.
_ORDERINGS = (
    ("__lt__", (("__gt__", _gt_from_lt), ("__le__", _le_from_lt), ("__ge__", _ge_from_lt))),
    ("__le__", (("__ge__", _ge_from_le), ("__lt__", _lt_from_le), ("__gt__", _gt_from_le))),
    ("__gt__", (("__lt__", _lt_from_gt), ("__ge__", _ge_from_gt), ("__le__", _le_from_gt))),
    ("__ge__", (("__le__", _le_from_ge), ("__gt__", _gt_from_ge), ("__lt__", _lt_from_ge))),
)

# A default no class attribute holds, so that a method set to None still reads as defined.
_UNSET = object()


def total_ordering(cls):
    """Class decorator supplying the ordering methods a class leaves undefined."""
    defined = []
    for name, built in _ORDERINGS:
        # Anything but object's own method counts, one inherited from a base class included.
        if getattr(cls, name, _UNSET) is not getattr(object, name, _UNSET):
            defined.append(name)
    if not defined:
        raise ValueError("must define at least one ordering operation: < > <= >=")
    for name, built in _ORDERINGS:
        if name == defined[0]:
            break
    for missing, method in built:
        if missing not in defined:
            method.__name__ = missing
            setattr(cls, missing, method)
    return cls
