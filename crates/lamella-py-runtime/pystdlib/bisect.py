"""Binary search for insertion points in sorted sequences, and sorted insertion.

bisect_left and bisect_right find where x belongs in the sorted sequence a,
before or after any entries equal to x; insort_left and insort_right insert it
there.  bisect and insort are the "right" variants.  Only the < operator is
used, on key(element) when a key function is given, so a never needs __eq__.
"""

# Every function takes a, x, lo and hi by position or by name, and key by name
# only.  CPython implements these functions in C, so a call that does not fit is
# reported by its argument parser rather than in a Python signature's words.
# Each function here binds its own arguments to raise the same errors, checked
# in the same order; the common call, with a and x alone, binds directly.
_PARAMETERS = ('a', 'x', 'lo', 'hi', 'key')

# The parameters a keyword may still name once the first n are given by position.
_NAMEABLE = [_PARAMETERS[given:] for given in range(5)]

# lo and hi must fit a signed 64-bit index, as they must in CPython on a 64-bit
# build.
_INDEX_MIN = -9223372036854775808
_INDEX_MAX = 9223372036854775807


def _as_plain_int(value):
    """The int an index-like value stands for, or None when it is not index-like."""
    if isinstance(value, int):
        return int(value)
    if not hasattr(type(value), '__index__'):
        return None
    result = value.__index__()
    if not isinstance(result, int):
        raise TypeError('__index__ returned non-int (type %s)' % type(result).__name__)
    return int(result)


def _checked_bounds(lo, hi):
    """lo and hi as plain ints, hi also allowed to stay None; anything else is a
    TypeError, and a value outside a 64-bit index an OverflowError."""
    value = _as_plain_int(lo)
    if value is None:
        raise TypeError("'%s' object cannot be interpreted as an integer" % type(lo).__name__)
    if value < _INDEX_MIN or value > _INDEX_MAX:
        raise OverflowError('Python int too large to convert to C ssize_t')
    lo = value
    if hi is not None:
        value = _as_plain_int(hi)
        if value is None:
            raise TypeError("argument should be integer or None, not '%s'" % type(hi).__name__)
        if value < _INDEX_MIN or value > _INDEX_MAX:
            raise OverflowError("cannot fit '%s' into an index-sized integer" % type(hi).__name__)
        hi = value
    return lo, hi


def _arguments(function, args, kwargs):
    """Bind one call of the named function to a, x, lo, hi and key, raising the
    first error CPython's parser would raise.  lo and hi come back as plain
    ints, and hi may also stay None."""
    given = len(args)
    if not kwargs and 1 < given < 5:
        if given == 2:
            return args[0], args[1], 0, None, None
        lo, hi = _checked_bounds(args[2], args[3] if given == 4 else None)
        return args[0], args[1], lo, hi, None
    if 1 < given < 5 and given + len(kwargs) < 6:
        # Each keyword must name a parameter not already given by position; when
        # one does not, _check_call raises the first error in the call.
        nameable = _NAMEABLE[given]
        for name in kwargs:
            if name not in nameable:
                _check_call(function, given, kwargs)
    else:
        _check_call(function, given, kwargs)
    a = args[0] if given > 0 else kwargs['a']
    x = args[1] if given > 1 else kwargs['x']
    lo = args[2] if given > 2 else kwargs.get('lo', 0)
    hi = args[3] if given > 3 else kwargs.get('hi')
    if given > 2 or 'lo' in kwargs or 'hi' in kwargs:
        lo, hi = _checked_bounds(lo, hi)
    return a, x, lo, hi, kwargs.get('key')


def _check_call(function, given, kwargs):
    """Raise the first error in a call of the named function with this many
    positional arguments and these keywords: too many arguments in all, too
    many by position, a required one missing, one given both ways, then an
    unknown keyword.  Return when the call binds."""
    total = given + len(kwargs)
    if total > 5:
        if given:
            raise TypeError(function + '() takes at most 5 arguments (' + str(total) + ' given)')
        raise TypeError(function + '() takes at most 5 keyword arguments (' + str(total) + ' given)')
    if given > 4:
        raise TypeError(function + '() takes at most 4 positional arguments (' + str(given) + ' given)')
    for position in range(given, 2):
        if _PARAMETERS[position] not in kwargs:
            raise TypeError(function + "() missing required argument '" + _PARAMETERS[position]
                            + "' (pos " + str(position + 1) + ')')
    for position in range(given):
        if _PARAMETERS[position] in kwargs:
            raise TypeError('argument for ' + function + "() given by name ('" + _PARAMETERS[position]
                            + "') and position (" + str(position + 1) + ')')
    for name in kwargs:
        if name not in _PARAMETERS:
            message = function + "() got an unexpected keyword argument '" + name + "'"
            meant = _suggestion(name)
            if meant is not None:
                message = message + ". Did you mean '" + meant + "'?"
            raise TypeError(message)


def _suggestion(word):
    """The parameter an unknown keyword most likely meant, or None.

    The cost of turning the keyword into a parameter's name is counted over
    their UTF-8 bytes: 2 to insert, delete or replace a byte, and 1 to replace
    an ASCII letter by the same letter in the other case.  A name qualifies when
    the cost is at most a third of the two lengths plus 3, rounded down; the
    cheapest qualifying name wins, the earliest parameter on a tie.
    """
    units = []
    for char in word:
        code = ord(char)
        if code < 0x80:
            units.append(code)
        else:
            # The bytes of a non-ASCII character match no byte of an ASCII
            # name, in either case, so -1 stands for each of them.
            width = 2 if code < 0x800 else 3 if code < 0x10000 else 4
            units.extend([-1] * width)
    best = None
    best_cost = 0
    for name in _PARAMETERS:
        cost = _edit_cost(units, name)
        if cost <= (len(units) + len(name) + 3) // 3 and (best is None or cost < best_cost):
            best = name
            best_cost = cost
    return best


def _edit_cost(units, name):
    """The cheapest edit from units to the ASCII name, at _suggestion's costs."""
    previous = list(range(0, 2 * len(name) + 1, 2))
    for unit in units:
        folded = unit + 32 if 65 <= unit <= 90 else unit
        current = [previous[0] + 2]
        for column in range(len(name)):
            code = ord(name[column])
            if unit == code:
                step = 0
            elif folded == (code + 32 if 65 <= code <= 90 else code):
                step = 1
            else:
                step = 2
            current.append(min(previous[column] + step, previous[column + 1] + 2, current[column] + 2))
        previous = current
    return previous[-1]


def _search_range(a, lo, hi):
    """Fill in the default hi and reject bounds and sequences a search cannot use."""
    if lo < 0:
        raise ValueError('lo must be non-negative')
    if hi is None or hi == -1:
        # -1 also stands for "up to the end", exactly like an absent hi.
        hi = len(a)
    if type(a) is dict:
        # The builtin dict is subscriptable yet is a mapping, not a sequence.
        raise TypeError('dict is not a sequence')
    if not hasattr(type(a), '__getitem__'):
        raise TypeError("'%s' object does not support indexing" % type(a).__name__)
    return lo, hi


def _leftmost(a, x, lo, hi, key):
    """Insertion point within [lo, hi) that keeps x before equal entries; bounds already checked."""
    while lo < hi:
        mid = (lo + hi) // 2
        if key is None:
            entry = a[mid]
        else:
            entry = key(a[mid])
        if entry < x:
            lo = mid + 1
        else:
            hi = mid
    return lo


def _rightmost(a, x, lo, hi, key):
    """Insertion point within [lo, hi) that keeps x after equal entries; bounds already checked."""
    while lo < hi:
        mid = (lo + hi) // 2
        if key is None:
            entry = a[mid]
        else:
            entry = key(a[mid])
        if x < entry:
            hi = mid
        else:
            lo = mid + 1
    return lo


def bisect_left(*args, **kwargs):
    """bisect_left(a, x, lo=0, hi=None, *, key=None)

    Return the index at which x goes into sorted a to stay before any entries
    equal to x.  Only a[lo:hi] is searched; hi defaults to len(a).  With key,
    the entries of a are compared through key(entry), while x itself is
    compared as given.
    """
    if kwargs or len(args) != 2:
        a, x, lo, hi, key = _arguments('bisect_left', args, kwargs)
    else:
        a, x = args
        lo = 0
        hi = None
        key = None
    lo, hi = _search_range(a, lo, hi)
    return _leftmost(a, x, lo, hi, key)


def bisect_right(*args, **kwargs):
    """bisect_right(a, x, lo=0, hi=None, *, key=None)

    Return the index at which x goes into sorted a to stay after any entries
    equal to x.  Only a[lo:hi] is searched; hi defaults to len(a).  With key,
    the entries of a are compared through key(entry), while x itself is
    compared as given.
    """
    if kwargs or len(args) != 2:
        a, x, lo, hi, key = _arguments('bisect_right', args, kwargs)
    else:
        a, x = args
        lo = 0
        hi = None
        key = None
    lo, hi = _search_range(a, lo, hi)
    return _rightmost(a, x, lo, hi, key)


def insort_left(*args, **kwargs):
    """insort_left(a, x, lo=0, hi=None, *, key=None)

    Insert x into sorted a before any entries equal to it, through a's own
    insert method.  With key, key(x) is what the search compares against
    key(entry); the insertion stores x itself.
    """
    if kwargs or len(args) != 2:
        a, x, lo, hi, key = _arguments('insort_left', args, kwargs)
    else:
        a, x = args
        lo = 0
        hi = None
        key = None
    if key is None:
        wanted = x
    else:
        wanted = key(x)
    lo, hi = _search_range(a, lo, hi)
    index = _leftmost(a, wanted, lo, hi, key)
    a.insert(index, x)


def insort_right(*args, **kwargs):
    """insort_right(a, x, lo=0, hi=None, *, key=None)

    Insert x into sorted a after any entries equal to it, through a's own
    insert method.  With key, key(x) is what the search compares against
    key(entry); the insertion stores x itself.
    """
    if kwargs or len(args) != 2:
        a, x, lo, hi, key = _arguments('insort_right', args, kwargs)
    else:
        a, x = args
        lo = 0
        hi = None
        key = None
    if key is None:
        wanted = x
    else:
        wanted = key(x)
    lo, hi = _search_range(a, lo, hi)
    index = _rightmost(a, wanted, lo, hi, key)
    a.insert(index, x)


bisect = bisect_right
insort = insort_right
