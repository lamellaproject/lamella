# The operator module, bundled as a MANAGED module -- the standard operators as functions, plus
# itemgetter / attrgetter / methodcaller. Transcribed from CPython's pure-Python operator.py (its C
# _operator accelerator is behaviorally identical), so the differential verifies this against the real
# module. NOT bundled: operator.abs -- it aliases builtins.abs, and our module-def hoisting would
# shadow the builtin before an `_abs = abs` capture runs (use the builtin abs directly). Everything
# else -- comparisons, arithmetic/bitwise, logical, get/set/del-item, containment, concat, index,
# length_hint, the in-place forms, and the three getter callables -- is here.

# Comparison
def lt(a, b):
    return a < b


def le(a, b):
    return a <= b


def eq(a, b):
    return a == b


def ne(a, b):
    return a != b


def ge(a, b):
    return a >= b


def gt(a, b):
    return a > b


# Logical
def not_(a):
    return not a


def truth(a):
    return True if a else False


def is_(a, b):
    return a is b


def is_not(a, b):
    return a is not b


def is_none(a):
    return a is None


def is_not_none(a):
    return a is not None


# Mathematical / bitwise (operator.abs is omitted -- it aliases builtins.abs, and our module-level def
# hoisting would shadow the builtin before an `_abs = abs` capture runs; use the builtin abs directly)
def add(a, b):
    return a + b


def and_(a, b):
    return a & b


def floordiv(a, b):
    return a // b


def inv(a):
    return ~a


invert = inv


def lshift(a, b):
    return a << b


def mod(a, b):
    return a % b


def mul(a, b):
    return a * b


def matmul(a, b):
    return a @ b


def neg(a):
    return -a


def or_(a, b):
    return a | b


def pos(a):
    return +a


def pow(a, b):
    return a ** b


def rshift(a, b):
    return a >> b


def sub(a, b):
    return a - b


def truediv(a, b):
    return a / b


def xor(a, b):
    return a ^ b


def index(a):
    return a.__index__()


# Sequence
def concat(a, b):
    if not hasattr(a, "__getitem__"):
        raise TypeError("'%s' object can't be concatenated" % type(a).__name__)
    return a + b


def contains(a, b):
    return b in a


def countOf(a, b):
    count = 0
    for i in a:
        if i is b or i == b:
            count += 1
    return count


def delitem(a, b):
    del a[b]


def getitem(a, b):
    return a[b]


def indexOf(a, b):
    for i, j in enumerate(a):
        if j is b or j == b:
            return i
    raise ValueError("sequence.index(x): x not in sequence")


def setitem(a, b, c):
    a[b] = c


def length_hint(obj, default=0):
    # An estimate of len(obj): exact via len() when available, else __length_hint__, else `default`.
    if not isinstance(default, int):
        raise TypeError(
            "'%s' object cannot be interpreted as an integer" % type(default).__name__
        )
    try:
        return len(obj)
    except TypeError:
        pass
    try:
        hint = type(obj).__length_hint__
    except AttributeError:
        return default
    try:
        val = hint(obj)
    except TypeError:
        return default
    if val is NotImplemented:
        return default
    if not isinstance(val, int):
        raise TypeError("__length_hint__ must be integer, not %s" % type(val).__name__)
    if val < 0:
        raise ValueError("__length_hint__() should return >= 0")
    return val


# Other
def call(obj, /, *args, **kwargs):
    return obj(*args, **kwargs)


class attrgetter:
    # attrgetter('name') -> f where f(r) == r.name; multiple names -> a tuple; dotted names descend.
    __slots__ = ("_attrs", "_call")

    def __init__(self, attr, /, *attrs):
        # The two branches use DISTINCT nested-function names (CPython reuses `func`): our frontend
        # miscompiles two same-named nested defs in sibling branches that capture different variables
        # (the second gets the first's closure). Distinct names sidestep it; behavior is identical.
        if not attrs:
            if not isinstance(attr, str):
                raise TypeError("attribute name must be a string")
            self._attrs = (attr,)
            names = attr.split(".")

            def get_dotted(obj):
                for name in names:
                    obj = getattr(obj, name)
                return obj

            self._call = get_dotted
        else:
            self._attrs = (attr,) + attrs
            getters = tuple(map(attrgetter, self._attrs))

            def get_many(obj):
                return tuple(getter(obj) for getter in getters)

            self._call = get_many

    def __call__(self, obj, /):
        return self._call(obj)

    def __repr__(self):
        return "%s.%s(%s)" % (
            self.__class__.__module__,
            self.__class__.__qualname__,
            ", ".join(map(repr, self._attrs)),
        )


class itemgetter:
    # itemgetter(2) -> f where f(r) == r[2]; multiple items -> a tuple of them.
    __slots__ = ("_items", "_call")

    def __init__(self, item, /, *items):
        # Distinct nested-function names per branch (see attrgetter) to sidestep the frontend
        # same-named-sibling-closure bug.
        if not items:
            self._items = (item,)

            def get_one(obj):
                return obj[item]

            self._call = get_one
        else:
            self._items = items = (item,) + items

            def get_many(obj):
                return tuple(obj[i] for i in items)

            self._call = get_many

    def __call__(self, obj, /):
        return self._call(obj)

    def __repr__(self):
        return "%s.%s(%s)" % (
            self.__class__.__module__,
            self.__class__.__name__,
            ", ".join(map(repr, self._items)),
        )


class methodcaller:
    # methodcaller('name', *args, **kw) -> f where f(r) == r.name(*args, **kw).
    __slots__ = ("_name", "_args", "_kwargs")

    def __init__(self, name, /, *args, **kwargs):
        self._name = name
        if not isinstance(self._name, str):
            raise TypeError("method name must be a string")
        self._args = args
        self._kwargs = kwargs

    def __call__(self, obj, /):
        return getattr(obj, self._name)(*self._args, **self._kwargs)

    def __repr__(self):
        args = [repr(self._name)]
        args.extend(map(repr, self._args))
        args.extend("%s=%r" % (k, v) for k, v in self._kwargs.items())
        return "%s.%s(%s)" % (
            self.__class__.__module__,
            self.__class__.__name__,
            ", ".join(args),
        )


# In-place forms (iconcat omitted; see the header)
def iadd(a, b):
    a += b
    return a


def iand(a, b):
    a &= b
    return a


def iconcat(a, b):
    if not hasattr(a, "__getitem__"):
        raise TypeError("'%s' object can't be concatenated" % type(a).__name__)
    a += b
    return a


def ifloordiv(a, b):
    a //= b
    return a


def ilshift(a, b):
    a <<= b
    return a


def imod(a, b):
    a %= b
    return a


def imul(a, b):
    a *= b
    return a


def imatmul(a, b):
    a @= b
    return a


def ior(a, b):
    a |= b
    return a


def ipow(a, b):
    a **= b
    return a


def irshift(a, b):
    a >>= b
    return a


def isub(a, b):
    a -= b
    return a


def itruediv(a, b):
    a /= b
    return a


def ixor(a, b):
    a ^= b
    return a
