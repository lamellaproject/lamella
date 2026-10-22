"""The intrinsic operators as plain functions, plus callable objects that fetch
attributes, fetch items or call a named method on their operand."""

__all__ = ['abs', 'add', 'and_', 'attrgetter', 'call', 'concat', 'contains', 'countOf',
           'delitem', 'eq', 'floordiv', 'ge', 'getitem', 'gt', 'iadd', 'iand', 'iconcat',
           'ifloordiv', 'ilshift', 'imatmul', 'imod', 'imul', 'index', 'indexOf', 'inv',
           'invert', 'ior', 'ipow', 'irshift', 'is_', 'is_none', 'is_not', 'is_not_none',
           'isub', 'itemgetter', 'itruediv', 'ixor', 'le', 'length_hint', 'lshift', 'lt',
           'matmul', 'methodcaller', 'mod', 'mul', 'ne', 'neg', 'not_', 'or_', 'pos', 'pow',
           'rshift', 'setitem', 'sub', 'truediv', 'truth', 'xor']

# Each function here takes its arguments through defaults it can tell from a real argument and
# raises the same errors itself; a call that fits passes one check.
_MISSING = object()

# An index-sized integer, as in CPython on a 64-bit build.
_SIZE_MAX = 2 ** 63 - 1
_SIZE_MIN = -2 ** 63

# The getters whose repr is being built, so that a getter holding itself prints as (...).
_active_reprs = []


def _refuse_keywords(name):
    raise TypeError('_operator.%s() takes no keyword arguments' % name)


def _one_argument_error(name, obj, extra, kwargs):
    """Raise the error of a one-argument function called with the wrong shape."""
    if kwargs:
        _refuse_keywords(name)
    given = len(extra)
    if obj is not _MISSING:
        given += 1
    raise TypeError('_operator.%s() takes exactly one argument (%d given)' % (name, given))


def _fixed_arguments_error(name, expected, values, extra, kwargs):
    """Raise the error of a fixed-arity function called with the wrong shape."""
    if kwargs:
        _refuse_keywords(name)
    given = len(extra)
    for value in values:
        if value is not _MISSING:
            given += 1
    plural = '' if expected == 1 else 's'
    raise TypeError('%s expected %d argument%s, got %d' % (name, expected, plural, given))


def _getter_call_error(name, obj, extra, kwargs):
    """Raise the error of an attrgetter or itemgetter called with the wrong shape."""
    if kwargs:
        raise TypeError('%s() takes no keyword arguments' % name)
    given = len(extra)
    if obj is not _MISSING:
        given += 1
    raise TypeError('%s expected 1 argument, got %d' % (name, given))


def lt(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a < b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('lt', 2, (a, b), extra, kwargs)
    return a < b


def le(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a <= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('le', 2, (a, b), extra, kwargs)
    return a <= b


def eq(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a == b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('eq', 2, (a, b), extra, kwargs)
    return a == b


def ne(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a != b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('ne', 2, (a, b), extra, kwargs)
    return a != b


def ge(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a >= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('ge', 2, (a, b), extra, kwargs)
    return a >= b


def gt(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a > b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('gt', 2, (a, b), extra, kwargs)
    return a > b


def not_(obj=_MISSING, /, *extra, **kwargs):
    """Return not obj."""
    if obj is _MISSING or extra or kwargs:
        _one_argument_error('not_', obj, extra, kwargs)
    return not obj


def truth(obj=_MISSING, /, *extra, **kwargs):
    """Return True when obj is true, else False."""
    if obj is _MISSING or extra or kwargs:
        _one_argument_error('truth', obj, extra, kwargs)
    return bool(obj)


def is_(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a is b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('is_', 2, (a, b), extra, kwargs)
    return a is b


def is_not(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a is not b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('is_not', 2, (a, b), extra, kwargs)
    return a is not b


def is_none(a=_MISSING, /, *extra, **kwargs):
    """Return a is None."""
    if a is _MISSING or extra or kwargs:
        _one_argument_error('is_none', a, extra, kwargs)
    return a is None


def is_not_none(a=_MISSING, /, *extra, **kwargs):
    """Return a is not None."""
    if a is _MISSING or extra or kwargs:
        _one_argument_error('is_not_none', a, extra, kwargs)
    return a is not None


def _make_abs(absolute):
    def abs(obj=_MISSING, /, *extra, **kwargs):
        """Return the absolute value of obj."""
        if obj is _MISSING or extra or kwargs:
            _one_argument_error('abs', obj, extra, kwargs)
        return absolute(obj)
    abs.__qualname__ = 'abs'
    return abs


# The built-in abs is read here, before this module binds the name to its own function. This
# interpreter binds a module-level def's name before the module body runs, so the function is made
# by assignment instead.
abs = _make_abs(abs)
del _make_abs


def add(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a + b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('add', 2, (a, b), extra, kwargs)
    return a + b


def and_(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a & b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('and_', 2, (a, b), extra, kwargs)
    return a & b


def floordiv(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a // b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('floordiv', 2, (a, b), extra, kwargs)
    return a // b


def index(a=_MISSING, /, *extra, **kwargs):
    """Return a as an exact int through its __index__ method."""
    if a is _MISSING or extra or kwargs:
        _one_argument_error('index', a, extra, kwargs)
    if isinstance(a, int):
        if type(a) is int:
            return a
        return int.__index__(a)
    method = getattr(type(a), '__index__', _MISSING)
    if method is _MISSING:
        raise TypeError("'%s' object cannot be interpreted as an integer" % type(a).__name__)
    result = method(a)
    if type(result) is int:
        return result
    if isinstance(result, int):
        # The warning goes where the warnings module sends it, on an interpreter that has one.
        try:
            import warnings
        except ImportError:
            pass
        else:
            warnings.warn('__index__ returned non-int (type %s).  The ability to return an '
                          'instance of a strict subclass of int is deprecated, and may be '
                          'removed in a future version of Python.'  # hygiene-allow: CPython's text
                          % type(result).__name__, DeprecationWarning)
        return int.__index__(result)
    raise TypeError('__index__ returned non-int (type %s)' % type(result).__name__)


def inv(obj=_MISSING, /, *extra, **kwargs):
    """Return ~obj."""
    if obj is _MISSING or extra or kwargs:
        _one_argument_error('inv', obj, extra, kwargs)
    return ~obj


def invert(obj=_MISSING, /, *extra, **kwargs):
    """Return ~obj."""
    if obj is _MISSING or extra or kwargs:
        _one_argument_error('invert', obj, extra, kwargs)
    return ~obj


def lshift(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a << b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('lshift', 2, (a, b), extra, kwargs)
    return a << b


def mod(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a % b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('mod', 2, (a, b), extra, kwargs)
    return a % b


def mul(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a * b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('mul', 2, (a, b), extra, kwargs)
    return a * b


def matmul(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a @ b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('matmul', 2, (a, b), extra, kwargs)
    return a @ b


def neg(obj=_MISSING, /, *extra, **kwargs):
    """Return -obj."""
    if obj is _MISSING or extra or kwargs:
        _one_argument_error('neg', obj, extra, kwargs)
    return -obj


def or_(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a | b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('or_', 2, (a, b), extra, kwargs)
    return a | b


def pos(obj=_MISSING, /, *extra, **kwargs):
    """Return +obj."""
    if obj is _MISSING or extra or kwargs:
        _one_argument_error('pos', obj, extra, kwargs)
    return +obj


def pow(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a ** b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('pow', 2, (a, b), extra, kwargs)
    return a ** b


def rshift(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a >> b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('rshift', 2, (a, b), extra, kwargs)
    return a >> b


def sub(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a - b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('sub', 2, (a, b), extra, kwargs)
    return a - b


def truediv(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a / b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('truediv', 2, (a, b), extra, kwargs)
    return a / b


def xor(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a ^ b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('xor', 2, (a, b), extra, kwargs)
    return a ^ b


def _is_sequence(obj):
    """True when obj supports item access and is not a mapping of the dict family."""
    return hasattr(type(obj), '__getitem__') and not isinstance(obj, dict)


def _has_native_concat(a, method):
    """True when a concatenates as a built-in sequence type does: a is one, or method is the
    concatenation its type inherits from one."""
    kind = type(a)
    if kind is list or kind is tuple or kind is str or kind is bytes or kind is bytearray:
        return True
    return (method is list.__add__ or method is tuple.__add__ or method is str.__add__
            or method is bytes.__add__ or method is bytearray.__add__)


def _add_by_protocol(a, b, left):
    """a + b through __add__ and __radd__ only; NotImplemented when neither side answers.

    A right operand whose type is a strict subclass of the left one's, with its own
    __radd__, is asked first.  Two operands of the same type never try __radd__."""
    left_type = type(a)
    right_type = type(b)
    right = _MISSING
    if right_type is not left_type:
        right = getattr(right_type, '__radd__', _MISSING)
        if (right is not _MISSING and issubclass(right_type, left_type)
                and getattr(left_type, '__radd__', _MISSING) is not right):
            result = right(b, a)
            if result is not NotImplemented:
                return result
            right = _MISSING
    if left is not _MISSING:
        result = left(a, b)
        if result is not NotImplemented:
            return result
    if right is not _MISSING:
        return right(b, a)
    return NotImplemented


def concat(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a + b for sequences a and b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('concat', 2, (a, b), extra, kwargs)
    left = getattr(type(a), '__add__', _MISSING)
    if _has_native_concat(a, left):
        return left(a, b)
    if _is_sequence(a) and _is_sequence(b):
        result = _add_by_protocol(a, b, left)
        if result is not NotImplemented:
            return result
    raise TypeError("'%s' object can't be concatenated" % type(a).__name__)


def contains(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return b in a."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('contains', 2, (a, b), extra, kwargs)
    return b in a


def countOf(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return how many items of a equal b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('countOf', 2, (a, b), extra, kwargs)
    try:
        items = iter(a)
    except TypeError:
        raise TypeError("argument of type '%s' is not iterable" % type(a).__name__) from None
    count = 0
    # next() keeps the single __iter__ call: a for loop would ask the iterator again.
    while True:
        try:
            item = next(items)
        except StopIteration:
            return count
        if item is b or item == b:
            count += 1


def delitem(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Delete a[b]."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('delitem', 2, (a, b), extra, kwargs)
    del a[b]


def getitem(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a[b]."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('getitem', 2, (a, b), extra, kwargs)
    return a[b]


def indexOf(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return the position of the first item of a that equals b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('indexOf', 2, (a, b), extra, kwargs)
    try:
        items = iter(a)
    except TypeError:
        raise TypeError("argument of type '%s' is not iterable" % type(a).__name__) from None
    position = 0
    while True:
        try:
            item = next(items)
        except StopIteration:
            raise ValueError('sequence.index(x): x not in sequence') from None
        if item is b or item == b:
            return position
        position += 1


def setitem(a=_MISSING, b=_MISSING, c=_MISSING, /, *extra, **kwargs):
    """Set a[b] to c."""
    if c is _MISSING or extra or kwargs:
        _fixed_arguments_error('setitem', 3, (a, b, c), extra, kwargs)
    a[b] = c


def length_hint(obj=_MISSING, default=0, /, *extra, **kwargs):
    """Return len(obj), else the estimate from obj's __length_hint__, else default."""
    if kwargs:
        _refuse_keywords('length_hint')
    if obj is _MISSING:
        raise TypeError('length_hint expected at least 1 argument, got 0')
    if extra:
        raise TypeError('length_hint expected at most 2 arguments, got %d' % (2 + len(extra)))
    if type(default) is not int:
        default = index(default)
    if default > _SIZE_MAX or default < _SIZE_MIN:
        raise OverflowError('Python int too large to convert to C ssize_t')
    try:
        return len(obj)
    except TypeError:
        pass
    try:
        kind = type(obj)
    except TypeError:
        return default + 0
    hint = getattr(kind, '__length_hint__', None)
    if hint is None:
        return default + 0
    try:
        result = hint(obj)
    except TypeError:
        return default + 0
    if result is NotImplemented:
        return default + 0
    if not isinstance(result, int):
        raise TypeError('__length_hint__ must be an integer, not %s' % type(result).__name__)
    if result > _SIZE_MAX or result < _SIZE_MIN:
        raise OverflowError('Python int too large to convert to C ssize_t')
    if result < 0:
        raise ValueError('__length_hint__() should return >= 0')
    if type(result) is not int:
        result = int.__index__(result)
    return result + 0


def call(obj=_MISSING, /, *args, **kwargs):
    """Return obj(*args, **kwargs)."""
    if obj is _MISSING:
        raise TypeError('call expected at least 1 argument, got 0')
    return obj(*args, **kwargs)


class attrgetter:
    """Callable that fetches one or more attributes, dotted paths included, from its operand."""

    __slots__ = ('_names', '_paths', '_single')

    def __new__(cls, *attrs, **kwargs):
        if kwargs:
            raise TypeError('attrgetter() takes no keyword arguments')
        if not attrs:
            raise TypeError('attrgetter expected 1 argument, got 0')
        paths = []
        for attr in attrs:
            if not isinstance(attr, str):
                raise TypeError('attribute name must be a string')
            paths.append(tuple(attr.split('.')))
        self = object.__new__(cls)
        self._names = attrs
        self._paths = tuple(paths)
        if len(attrs) == 1 and len(paths[0]) == 1:
            self._single = attrs[0]
        else:
            self._single = None
        return self

    def __init_subclass__(cls, **kwargs):
        raise TypeError("type 'operator.attrgetter' is not an acceptable base type")

    def __call__(self, obj=_MISSING, /, *extra, **kwargs):
        if kwargs or extra or obj is _MISSING:
            _getter_call_error('attrgetter', obj, extra, kwargs)
        name = self._single
        if name is not None:
            return getattr(obj, name)
        values = []
        for path in self._paths:
            value = obj
            for part in path:
                value = getattr(value, part)
            values.append(value)
        if len(values) == 1:
            return values[0]
        return tuple(values)

    def __repr__(self):
        return 'operator.attrgetter(%s)' % ', '.join([repr(name) for name in self._names])

    def __reduce__(self):
        return (type(self), self._names)

    def __getstate__(self):
        return None


class itemgetter:
    """Callable that fetches one or more items from its operand."""

    __slots__ = ('_items', '_item')

    def __new__(cls, *items, **kwargs):
        if kwargs:
            raise TypeError('itemgetter() takes no keyword arguments')
        if not items:
            raise TypeError('itemgetter expected 1 argument, got 0')
        self = object.__new__(cls)
        self._items = items
        self._item = items[0] if len(items) == 1 else _MISSING
        return self

    def __init_subclass__(cls, **kwargs):
        raise TypeError("type 'operator.itemgetter' is not an acceptable base type")

    def __call__(self, obj=_MISSING, /, *extra, **kwargs):
        if kwargs or extra or obj is _MISSING:
            _getter_call_error('itemgetter', obj, extra, kwargs)
        item = self._item
        if item is not _MISSING:
            return obj[item]
        return tuple([obj[item] for item in self._items])

    def __repr__(self):
        key = id(self)
        if key in _active_reprs:
            return 'operator.itemgetter(...)'
        _active_reprs.append(key)
        try:
            return 'operator.itemgetter(%s)' % ', '.join([repr(item) for item in self._items])
        finally:
            _active_reprs.remove(key)

    def __reduce__(self):
        return (type(self), self._items)

    def __getstate__(self):
        return None


class methodcaller:
    """Callable that calls a named method of its operand with stored arguments."""

    __slots__ = ('_name', '_args', '_kwargs')

    def __new__(cls, *args, **kwargs):
        if not args:
            raise TypeError('methodcaller needs at least one argument, the method name')
        name = args[0]
        if not isinstance(name, str):
            raise TypeError('method name must be a string')
        self = object.__new__(cls)
        self._name = name
        self._args = args[1:]
        self._kwargs = kwargs
        return self

    def __init_subclass__(cls, **kwargs):
        raise TypeError("type 'operator.methodcaller' is not an acceptable base type")

    def __call__(self, obj=_MISSING, /, *extra, **kwargs):
        if obj is _MISSING or extra:
            given = len(extra)
            if obj is not _MISSING:
                given += 1
            raise TypeError('methodcaller expected 1 argument, got %d' % given)
        if kwargs:
            raise TypeError('methodcaller() takes no keyword arguments')
        return getattr(obj, self._name)(*self._args, **self._kwargs)

    def __repr__(self):
        key = id(self)
        if key in _active_reprs:
            return 'operator.methodcaller(...)'
        _active_reprs.append(key)
        try:
            parts = [repr(self._name)]
            for arg in self._args:
                parts.append(repr(arg))
            for keyword, value in self._kwargs.items():
                parts.append('%s=%r' % (keyword, value))
            return 'operator.methodcaller(%s)' % ', '.join(parts)
        finally:
            _active_reprs.remove(key)

    def __reduce__(self):
        if not self._kwargs:
            return (type(self), (self._name,) + self._args)
        from functools import partial
        return (partial(type(self), self._name, **self._kwargs), self._args)

    def __getstate__(self):
        return None


def iadd(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a += b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('iadd', 2, (a, b), extra, kwargs)
    a += b
    return a


def iand(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a &= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('iand', 2, (a, b), extra, kwargs)
    a &= b
    return a


def iconcat(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a += b, for sequences a and b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('iconcat', 2, (a, b), extra, kwargs)
    left_type = type(a)
    inplace = getattr(left_type, '__iadd__', _MISSING)
    if left_type is list or left_type is bytearray:
        if inplace is _MISSING:
            # This interpreter extends a list or bytearray in place without naming the method on
            # the type.
            a += b
            return a
        return inplace(a, b)
    if inplace is not _MISSING and (inplace is getattr(list, '__iadd__', None)
                                    or inplace is getattr(bytearray, '__iadd__', None)):
        return inplace(a, b)
    left = getattr(left_type, '__add__', _MISSING)
    if _has_native_concat(a, left):
        return left(a, b)
    if _is_sequence(a) and _is_sequence(b):
        if inplace is not _MISSING:
            result = inplace(a, b)
            if result is not NotImplemented:
                return result
        result = _add_by_protocol(a, b, left)
        if result is not NotImplemented:
            return result
    raise TypeError("'%s' object can't be concatenated" % left_type.__name__)


def ifloordiv(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a //= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('ifloordiv', 2, (a, b), extra, kwargs)
    a //= b
    return a


def ilshift(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a <<= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('ilshift', 2, (a, b), extra, kwargs)
    a <<= b
    return a


def imod(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a %= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('imod', 2, (a, b), extra, kwargs)
    a %= b
    return a


def imul(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a *= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('imul', 2, (a, b), extra, kwargs)
    a *= b
    return a


def imatmul(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a @= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('imatmul', 2, (a, b), extra, kwargs)
    a @= b
    return a


def ior(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a |= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('ior', 2, (a, b), extra, kwargs)
    a |= b
    return a


def ipow(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a **= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('ipow', 2, (a, b), extra, kwargs)
    a **= b
    return a


def irshift(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a >>= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('irshift', 2, (a, b), extra, kwargs)
    a >>= b
    return a


def isub(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a -= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('isub', 2, (a, b), extra, kwargs)
    a -= b
    return a


def itruediv(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a /= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('itruediv', 2, (a, b), extra, kwargs)
    a /= b
    return a


def ixor(a=_MISSING, b=_MISSING, /, *extra, **kwargs):
    """Return a after a ^= b."""
    if b is _MISSING or extra or kwargs:
        _fixed_arguments_error('ixor', 2, (a, b), extra, kwargs)
    a ^= b
    return a


__lt__ = lt
__le__ = le
__eq__ = eq
__ne__ = ne
__ge__ = ge
__gt__ = gt
__not__ = not_
__abs__ = abs
__add__ = add
__and__ = and_
__call__ = call
__floordiv__ = floordiv
__index__ = index
__inv__ = inv
__invert__ = invert
__lshift__ = lshift
__mod__ = mod
__mul__ = mul
__matmul__ = matmul
__neg__ = neg
__or__ = or_
__pos__ = pos
__pow__ = pow
__rshift__ = rshift
__sub__ = sub
__truediv__ = truediv
__xor__ = xor
__concat__ = concat
__contains__ = contains
__delitem__ = delitem
__getitem__ = getitem
__setitem__ = setitem
__iadd__ = iadd
__iand__ = iand
__iconcat__ = iconcat
__ifloordiv__ = ifloordiv
__ilshift__ = ilshift
__imod__ = imod
__imul__ = imul
__imatmul__ = imatmul
__ior__ = ior
__ipow__ = ipow
__irshift__ = irshift
__isub__ = isub
__itruediv__ = itruediv
__ixor__ = ixor
