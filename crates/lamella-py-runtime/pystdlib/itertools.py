"""Iterators for efficient looping: counters and repeaters that may never end, tools that
filter, slice, group and join other iterables, and the combinatoric generators."""

# Each tool binds its own arguments, once, when it is built, and raises the same errors; __next__
# does only the work of one step.

# The bound of an index-sized integer, as in CPython on a 64-bit build.
_MAX = 9223372036854775807
_MISSING = object()
# How many elements one block of tee's shared buffer holds.
_BLOCK = 64


def _self(self):
    return self


def _unpicklable(self, protocol=0):
    kind = type(self)
    name = kind.__name__
    if protocol >= 2 and kind.__module__ == 'itertools':
        name = 'itertools.' + name
    raise TypeError("cannot pickle '%s' object" % name)


def _index(value):
    if isinstance(value, int):
        number = int(value)
    else:
        method = getattr(type(value), '__index__', None)
        if method is None:
            raise TypeError("'%s' object cannot be interpreted as an integer" % type(value).__name__)
        number = method(value)
        if not isinstance(number, int):
            raise TypeError("__index__ returned non-int (type %s)" % type(number).__name__)
    if number > _MAX or number < -_MAX - 1:
        raise OverflowError("Python int too large to convert to C ssize_t")
    return number


def _is_number(value):
    if isinstance(value, (int, float, complex)):
        return True
    kind = type(value)
    return hasattr(kind, '__index__') or hasattr(kind, '__int__') or hasattr(kind, '__float__')


def _distance(a, b):
    """Edit distance: inserting, deleting or replacing a character costs 2, changing only its case costs 1."""
    previous = list(range(0, 2 * len(b) + 1, 2))
    for i in range(len(a)):
        current = [2 * (i + 1)]
        for j in range(len(b)):
            if a[i] == b[j]:
                cost = previous[j]
            elif a[i].lower() == b[j].lower():
                cost = previous[j] + 1
            else:
                cost = previous[j] + 2
            current.append(min(cost, previous[j + 1] + 2, current[j] + 2))
        previous = current
    return previous[-1]


def _unexpected(name, key, names):
    message = "%s() got an unexpected keyword argument '%s'" % (name, key)
    best = None
    for candidate in names:
        cost = _distance(key, candidate)
        if cost <= (len(key) + len(candidate) + 3) * 2 // 6 and (best is None or cost < best_cost):
            best, best_cost = candidate, cost
    if best is not None:
        message += ". Did you mean '%s'?" % best
    raise TypeError(message)


def _parse(name, names, required, positional, args, kwargs):
    given = len(args)
    total = len(names)
    if given + len(kwargs) > total:
        raise TypeError("%s() takes at most %d arguments (%d given)" % (name, total, given + len(kwargs)))
    if given > positional:
        raise TypeError("%s() takes %s %d positional arguments (%d given)"
                        % (name, "exactly" if required == positional else "at most", positional, given))
    values = list(args)
    for i in range(given, total):
        key = names[i]
        if key in kwargs:
            values.append(kwargs.pop(key))
        elif i < required:
            raise TypeError("%s() missing required argument '%s' (pos %d)" % (name, key, i + 1))
        else:
            values.append(_MISSING)
    for key in kwargs:
        if key in names[:given]:
            raise TypeError("argument for %s() given by name ('%s') and position (%d)"
                            % (name, key, names.index(key) + 1))
    for key in kwargs:
        _unexpected(name, key, names)
    return values


def _exact(name, count, args, kwargs):
    if kwargs:
        raise TypeError("%s() takes no keyword arguments" % name)
    if len(args) != count:
        raise TypeError("%s expected %d argument%s, got %d" % (name, count, "" if count == 1 else "s", len(args)))


def _slice_arg(value, message):
    try:
        number = _index(value)
    except (TypeError, OverflowError):
        number = -1
    if number == -1:
        raise ValueError(message)
    return number


class accumulate:
    """Running results of a two-argument function (addition by default) over the iterable."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        iterable, func, initial = _parse('accumulate', ('iterable', 'func', 'initial'), 1, 2, args, kwargs)
        self = object.__new__(cls)
        self._it = iter(iterable)
        self._func = None if func is _MISSING else func
        self._pending = None if initial is _MISSING else initial
        self._total = _MISSING
        return self

    def __next__(self):
        pending = self._pending
        if pending is not None:
            self._pending = None
            self._total = pending
            return pending
        value = next(self._it)
        total = self._total
        if total is _MISSING:
            self._total = value
            return value
        func = self._func
        total = total + value if func is None else func(total, value)
        self._total = total
        return total


class batched:
    """Tuples of n consecutive elements; only the last one may be shorter."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        iterable, n, strict = _parse('batched', ('iterable', 'n', 'strict'), 2, 2, args, kwargs)
        n = _index(n)
        strict = strict is not _MISSING and bool(strict)
        if n < 1:
            raise ValueError("n must be at least one")
        self = object.__new__(cls)
        self._it = iter(iterable)
        self._n = n
        self._strict = strict
        return self

    def __next__(self):
        it = self._it
        if it is None:
            raise StopIteration
        batch = []
        left = self._n
        try:
            while left:
                batch.append(next(it))
                left -= 1
        except StopIteration:
            left = -1
        except BaseException:
            self._it = None
            raise
        if left < 0:
            if batch and not self._strict:
                return tuple(batch)
            self._it = None
            if batch:
                raise ValueError("batched(): incomplete batch")
            raise StopIteration
        return tuple(batch)


class chain:
    """The elements of each iterable in turn, each iterable started only when reached."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *sources, **kwargs):
        if kwargs:
            raise TypeError("chain() takes no keyword arguments")
        self = object.__new__(cls)
        self._sources = iter(sources)
        self._active = None
        return self

    def __class_getitem__(cls, item):
        return type(list[int])(cls, item)

    @classmethod
    def from_iterable(cls, *args, **kwargs):
        if kwargs:
            raise TypeError("chain.from_iterable() takes no keyword arguments")
        if len(args) != 1:
            raise TypeError("chain.from_iterable() takes exactly one argument (%d given)" % len(args))
        self = object.__new__(cls)
        self._sources = iter(args[0])
        self._active = None
        return self

    def __next__(self):
        active = self._active
        while True:
            if active is not None:
                try:
                    return next(active)
                except StopIteration:
                    active = self._active = None
            sources = self._sources
            if sources is None:
                raise StopIteration
            try:
                source = next(sources)
            except StopIteration:
                source = _MISSING
            except BaseException:
                self._sources = None
                raise
            if source is _MISSING:
                self._sources = None
                raise StopIteration
            try:
                active = self._active = iter(source)
            except BaseException:
                self._sources = None
                raise


class combinations:
    """r-length subsequences of the iterable, in lexicographic order of positions."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        iterable, r = _parse('combinations', ('iterable', 'r'), 2, 2, args, kwargs)
        r = _index(r)
        items = tuple(iterable)
        if r < 0:
            raise ValueError("r must be non-negative")
        self = object.__new__(cls)
        self._items = items
        self._r = r
        self._picks = None
        return self

    def __next__(self):
        items = self._items
        if items is None:
            raise StopIteration
        r = self._r
        picks = self._picks
        if picks is None:
            if r > len(items):
                self._items = None
                raise StopIteration
            self._picks = picks = list(range(r))
        else:
            # Advance the rightmost position that can still move, and close up the ones after it.
            slack = len(items) - r
            i = r - 1
            while i >= 0 and picks[i] == slack + i:
                i -= 1
            if i < 0:
                self._items = None
                raise StopIteration
            position = picks[i]
            while i < r:
                position += 1
                picks[i] = position
                i += 1
        return tuple([items[k] for k in picks])


class combinations_with_replacement:
    """r-length subsequences of the iterable whose positions may repeat, in lexicographic order."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        iterable, r = _parse('combinations_with_replacement', ('iterable', 'r'), 2, 2, args, kwargs)
        r = _index(r)
        items = tuple(iterable)
        if r < 0:
            raise ValueError("r must be non-negative")
        self = object.__new__(cls)
        self._items = items
        self._r = r
        self._picks = None
        return self

    def __next__(self):
        items = self._items
        if items is None:
            raise StopIteration
        r = self._r
        picks = self._picks
        if picks is None:
            if r and not items:
                self._items = None
                raise StopIteration
            self._picks = picks = [0] * r
        else:
            # Advance the rightmost position not yet on the last element, and level the ones after it.
            top = len(items) - 1
            i = r - 1
            while i >= 0 and picks[i] == top:
                i -= 1
            if i < 0:
                self._items = None
                raise StopIteration
            position = picks[i] + 1
            while i < r:
                picks[i] = position
                i += 1
        return tuple([items[k] for k in picks])


class compress:
    """The elements of data whose matching selector is true."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        data, selectors = _parse('compress', ('data', 'selectors'), 2, 2, args, kwargs)
        self = object.__new__(cls)
        self._data = iter(data)
        self._selectors = iter(selectors)
        return self

    def __next__(self):
        data = self._data
        selectors = self._selectors
        while True:
            item = next(data)
            if next(selectors):
                return item


class count:
    """start, start + step, start + 2 * step, ... without end."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        start, step = _parse('count', ('start', 'step'), 0, 2, args, kwargs)
        if start is _MISSING:
            start = 0
        elif not _is_number(start):
            raise TypeError("a number is required")
        if step is _MISSING:
            step = 1
        elif not _is_number(step):
            raise TypeError("a number is required")
        if isinstance(start, int) and isinstance(step, int) and step == 1 and -_MAX - 1 <= start <= _MAX:
            start = int(start)
        self = object.__new__(cls)
        self._value = start
        self._step = step
        return self

    def __next__(self):
        value = self._value
        self._value = value + self._step
        return value

    def __repr__(self):
        step = self._step
        if isinstance(step, int) and step == 1:
            return '%s(%s)' % (type(self).__name__, repr(self._value))
        return '%s(%s, %s)' % (type(self).__name__, repr(self._value), repr(step))


class cycle:
    """The iterable's elements, then a saved copy of them, forever."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        _exact('cycle', 1, args, kwargs)
        self = object.__new__(cls)
        self._it = iter(args[0])
        self._saved = []
        self._pos = 0
        return self

    def __next__(self):
        it = self._it
        if it is not None:
            try:
                item = next(it)
            except StopIteration:
                self._it = None
            else:
                self._saved.append(item)
                return item
        saved = self._saved
        if not saved:
            raise StopIteration
        pos = self._pos
        item = saved[pos]
        pos += 1
        self._pos = 0 if pos == len(saved) else pos
        return item


class dropwhile:
    """Every element from the first one that fails the predicate."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        _exact('dropwhile', 2, args, kwargs)
        self = object.__new__(cls)
        self._predicate = args[0]
        self._it = iter(args[1])
        return self

    def __next__(self):
        it = self._it
        predicate = self._predicate
        if predicate is _MISSING:
            return next(it)
        while True:
            item = next(it)
            if not predicate(item):
                self._predicate = _MISSING
                return item


class filterfalse:
    """The elements for which the predicate is false; with None, the false elements."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        _exact('filterfalse', 2, args, kwargs)
        self = object.__new__(cls)
        self._predicate = args[0]
        self._it = iter(args[1])
        return self

    def __next__(self):
        it = self._it
        predicate = self._predicate
        while True:
            item = next(it)
            if predicate is None:
                if not item:
                    return item
            elif not predicate(item):
                return item


class groupby:
    """(key, group) pairs for each run of consecutive elements with equal keys."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        iterable, key = _parse('groupby', ('iterable', 'key'), 1, 2, args, kwargs)
        self = object.__new__(cls)
        self._it = iter(iterable)
        self._keyof = None if key is _MISSING else key
        self._buffered = False
        self._held = None
        self._heldkey = None
        self._runkey = _MISSING
        self._live = None
        return self

    def _fetch(self):
        try:
            value = next(self._it)
        except StopIteration:
            return False
        keyof = self._keyof
        self._heldkey = value if keyof is None else keyof(value)
        self._held = value
        self._buffered = True
        return True

    def __next__(self):
        # The group handed out last goes dead first; then the rest of its run is skipped.
        self._live = None
        while True:
            if not self._buffered and not self._fetch():
                raise StopIteration
            key = self._heldkey
            runkey = self._runkey
            if runkey is _MISSING or not (runkey is key or runkey == key):
                break
            self._buffered = False
        self._runkey = key
        group = self._live = _grouper(self, key)
        return (key, group)


class _grouper:
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __init_subclass__(cls, **kwargs):
        raise TypeError("type 'itertools._grouper' is not an acceptable base type")

    def __new__(cls, owner, runkey):
        self = object.__new__(cls)
        self._owner = owner
        self._runkey = runkey
        return self

    def __next__(self):
        owner = self._owner
        if owner._live is not self:
            raise StopIteration
        if not owner._buffered and not owner._fetch():
            raise StopIteration
        key = owner._heldkey
        runkey = self._runkey
        if not (runkey is key or runkey == key):
            raise StopIteration
        owner._buffered = False
        return owner._held


class islice:
    """The elements at positions start, start + step, ... below stop; nothing negative allowed."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        if kwargs:
            raise TypeError("islice() takes no keyword arguments")
        given = len(args)
        if given < 2:
            raise TypeError("islice expected at least 2 arguments, got %d" % given)
        if given > 4:
            raise TypeError("islice expected at most 4 arguments, got %d" % given)
        start = 0
        step = 1
        stop = args[1] if given == 2 else args[2]
        if stop is not None:
            stop = _slice_arg(stop, "Stop argument for islice() must be None or an integer: 0 <= x <= sys.maxsize.")
        if given > 2 and args[1] is not None:
            start = _slice_arg(args[1], "Indices for islice() must be None or an integer: 0 <= x <= sys.maxsize.")
        if start < 0 or (stop is not None and stop < 0):
            raise ValueError("Indices for islice() must be None or an integer: 0 <= x <= sys.maxsize.")
        if given == 4 and args[3] is not None:
            step = _slice_arg(args[3], "Step for islice() must be a positive integer or None.")
        if step < 1:
            raise ValueError("Step for islice() must be a positive integer or None.")
        self = object.__new__(cls)
        self._it = iter(args[0])
        self._due = start
        self._stop = stop
        self._step = step
        self._seen = 0
        return self

    def __next__(self):
        it = self._it
        if it is None:
            raise StopIteration
        seen = self._seen
        due = self._due
        stop = self._stop
        try:
            while seen < due:
                next(it)
                seen += 1
            if stop is None or seen < stop:
                item = next(it)
            else:
                item = _MISSING
        except BaseException:
            self._it = None
            raise
        if item is _MISSING:
            self._it = None
            raise StopIteration
        self._seen = seen + 1
        # A position that would pass stop becomes stop, so the input is never read beyond it. With
        # no stop, a position past sys.maxsize becomes -1, as in CPython: the element after this one
        # comes next, and the count goes on from -1.
        due += self._step
        if stop is None:
            if due > _MAX:
                due = -1
        elif due > stop:
            due = stop
        self._due = due
        return item


class pairwise:
    """Overlapping pairs of consecutive elements."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        _exact('pairwise', 1, args, kwargs)
        self = object.__new__(cls)
        self._it = iter(args[0])
        self._old = _MISSING
        return self

    def __next__(self):
        it = self._it
        if it is None:
            raise StopIteration
        old = self._old
        try:
            if old is _MISSING:
                old = self._old = next(it)
            new = next(it)
        except BaseException:
            self._it = None
            self._old = None
            raise
        self._old = new
        return (old, new)


class permutations:
    """r-length orderings of the iterable's elements, in lexicographic order of positions."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        iterable, r = _parse('permutations', ('iterable', 'r'), 1, 2, args, kwargs)
        items = tuple(iterable)
        if r is _MISSING or r is None:
            r = len(items)
        elif not isinstance(r, int):
            raise TypeError("Expected int as r")
        else:
            r = int(r)
            if r > _MAX or r < -_MAX - 1:
                raise OverflowError("Python int too large to convert to C ssize_t")
            if r < 0:
                raise ValueError("r must be non-negative")
        self = object.__new__(cls)
        self._items = items
        self._r = r
        self._picks = None
        self._used = None
        return self

    def __next__(self):
        items = self._items
        if items is None:
            raise StopIteration
        r = self._r
        n = len(items)
        picks = self._picks
        if picks is None:
            if r > n:
                self._items = None
                raise StopIteration
            self._picks = picks = list(range(r))
            self._used = [k < r for k in range(n)]
        else:
            used = self._used
            i = r - 1
            while i >= 0:
                # Free position i, then look for a larger position still unused.
                j = picks[i]
                used[j] = False
                j += 1
                while j < n and used[j]:
                    j += 1
                if j < n:
                    picks[i] = j
                    used[j] = True
                    # Refill the positions after i with the smallest unused ones, in order.
                    k = 0
                    i += 1
                    while i < r:
                        while used[k]:
                            k += 1
                        picks[i] = k
                        used[k] = True
                        i += 1
                    break
                i -= 1
            if i < 0:
                self._items = None
                raise StopIteration
        return tuple([items[k] for k in picks])


class product:
    """The cartesian product of the iterables, like nested for-loops."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *sources, **kwargs):
        times = 1
        if kwargs:
            if len(kwargs) > 1:
                raise TypeError("product() takes at most 1 keyword argument (%d given)" % len(kwargs))
            for key in kwargs:
                if key != 'repeat':
                    _unexpected('product', key, ('repeat',))
            times = _index(kwargs['repeat'])
            if times < 0:
                raise ValueError("repeat argument cannot be negative")
            # A repeat that would make 2**60 (1152921504606846976) or more pools is refused before
            # any input is read.
            if len(sources) * times >= 1152921504606846976:
                raise OverflowError("repeat argument too large")
        axes = []
        if times:
            for source in sources:
                axes.append(tuple(source))
            axes = axes * times
        self = object.__new__(cls)
        self._axes = axes
        self._digits = None
        self._current = None
        return self

    def __next__(self):
        axes = self._axes
        if axes is None:
            raise StopIteration
        current = self._current
        if current is None:
            for axis in axes:
                if not axis:
                    self._axes = None
                    raise StopIteration
            self._current = current = [axis[0] for axis in axes]
            self._digits = [0] * len(axes)
        else:
            # Count like an odometer: the last axis turns fastest.
            digits = self._digits
            i = len(axes) - 1
            while i >= 0:
                axis = axes[i]
                j = digits[i] + 1
                if j < len(axis):
                    digits[i] = j
                    current[i] = axis[j]
                    break
                digits[i] = 0
                current[i] = axis[0]
                i -= 1
            if i < 0:
                self._axes = None
                raise StopIteration
        return tuple(current)


class repeat:
    """The same object again and again, endlessly or a given number of times."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        item, times = _parse('repeat', ('object', 'times'), 1, 2, args, kwargs)
        if times is _MISSING:
            times = None
        else:
            times = _index(times)
            if times < 0:
                times = 0
        self = object.__new__(cls)
        self._item = item
        self._left = times
        return self

    def __next__(self):
        left = self._left
        if left is None:
            return self._item
        if left <= 0:
            raise StopIteration
        self._left = left - 1
        return self._item

    def __repr__(self):
        if self._left is None:
            return '%s(%s)' % (type(self).__name__, repr(self._item))
        return '%s(%s, %d)' % (type(self).__name__, repr(self._item), self._left)

    def __length_hint__(self):
        if self._left is None:
            raise TypeError("len() of unsized object")
        return self._left


class starmap:
    """The function called with each element's items as its arguments."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        _exact('starmap', 2, args, kwargs)
        self = object.__new__(cls)
        self._func = args[0]
        self._it = iter(args[1])
        return self

    def __next__(self):
        return self._func(*tuple(next(self._it)))


class takewhile:
    """The leading elements for which the predicate holds."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *args, **kwargs):
        _exact('takewhile', 2, args, kwargs)
        self = object.__new__(cls)
        self._predicate = args[0]
        self._it = iter(args[1])
        return self

    def __next__(self):
        it = self._it
        if it is None:
            raise StopIteration
        item = next(it)
        if self._predicate(item):
            return item
        self._it = None
        raise StopIteration


class _tee:
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __init_subclass__(cls, **kwargs):
        raise TypeError("type 'itertools._tee' is not an acceptable base type")

    def __new__(cls, iterable):
        self = object.__new__(cls)
        self._source = iter(iterable)
        self._lock = [False]
        # A block is a list: the next block (or None) at index 0, then buffered elements.
        self._block = [None]
        self._position = 1
        return self

    def __copy__(self):
        twin = object.__new__(_tee)
        twin._source = self._source
        twin._lock = self._lock
        twin._block = self._block
        twin._position = self._position
        return twin

    def __next__(self):
        block = self._block
        position = self._position
        if position > _BLOCK:
            following = block[0]
            if following is None:
                following = block[0] = [None]
            self._block = block = following
            position = 1
        if position < len(block):
            item = block[position]
        else:
            lock = self._lock
            if lock[0]:
                raise RuntimeError("cannot re-enter the tee iterator")
            lock[0] = True
            try:
                item = next(self._source)
            except StopIteration:
                item = _MISSING
            finally:
                lock[0] = False
            if item is _MISSING:
                raise StopIteration
            block.append(item)
        self._position = position + 1
        return item


def tee(*args, **kwargs):
    """n independent iterators over one iterable, sharing what they have read."""
    if kwargs:
        raise TypeError("itertools.tee() takes no keyword arguments")
    given = len(args)
    if given < 1:
        raise TypeError("tee expected at least 1 argument, got 0")
    if given > 2:
        raise TypeError("tee expected at most 2 arguments, got %d" % given)
    n = 2 if given == 1 else _index(args[1])
    if n < 0:
        raise ValueError("n must be >= 0")
    if n == 0:
        return ()
    source = iter(args[0])
    head = source.__copy__() if isinstance(source, _tee) else _tee(source)
    # The whole tuple is sized first, so an n too large to hold fails before any copy is made.
    copies = [head] * n
    for k in range(1, n):
        copies[k] = head.__copy__()
    return tuple(copies)


class zip_longest:
    """Tuples of matching elements from every iterable, the shorter ones padded with fillvalue."""
    __iter__ = _self
    __reduce__ = __reduce_ex__ = _unpicklable

    def __new__(cls, *sources, **kwargs):
        fill = None
        if kwargs:
            if len(kwargs) != 1 or 'fillvalue' not in kwargs:
                raise TypeError("zip_longest() got an unexpected keyword argument")
            fill = kwargs['fillvalue']
        self = object.__new__(cls)
        self._its = [iter(source) for source in sources]
        self._fill = fill
        return self

    def __next__(self):
        its = self._its
        if its is None:
            raise StopIteration
        fill = self._fill
        row = []
        alive = False
        # An input that has stopped is left as None and is never called again.
        for k in range(len(its)):
            it = its[k]
            value = fill
            if it is not None:
                try:
                    value = next(it)
                    alive = True
                except StopIteration:
                    its[k] = None
                except BaseException:
                    self._its = None
                    raise
            row.append(value)
        if not alive:
            self._its = None
            raise StopIteration
        return tuple(row)
