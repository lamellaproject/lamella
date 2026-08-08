"""Iterator building blocks, after the CPython itertools reference implementations.

CPython implements these in C; each is documented with a pure-Python equivalent, and
that equivalent is what this module realizes, and its observable behaviour is verified
against CPython's own itertools.

Known deltas from the C module: these are generator functions and plain classes, so
`type(count(1))` is a generator, not `itertools.count`, and the C types' pickling and
copy support is absent. Callers observe the yielded values, which agree.
"""

import operator


def count(start=0, step=1):
    n = start
    while True:
        yield n
        n += step


def cycle(iterable):
    saved = []
    for element in iterable:
        yield element
        saved.append(element)

    while saved:
        for element in saved:
            yield element


def repeat(object, times=None):
    if times is None:
        while True:
            yield object
    else:
        for _ in range(times):
            yield object


def accumulate(iterable, function=operator.add, *, initial=None):
    iterator = iter(iterable)
    total = initial
    if initial is None:
        for first in iterator:
            total = first
            break
        else:
            return

    yield total
    for element in iterator:
        total = function(total, element)
        yield total


def chain(*iterables):
    for iterable in iterables:
        yield from iterable


def _chain_from_iterable(iterables):
    for iterable in iterables:
        yield from iterable


chain.from_iterable = _chain_from_iterable


def compress(data, selectors):
    for datum, selector in zip(data, selectors):
        if selector:
            yield datum


def dropwhile(predicate, iterable):
    iterator = iter(iterable)
    for x in iterator:
        if not predicate(x):
            yield x
            break

    for x in iterator:
        yield x


def takewhile(predicate, iterable):
    for x in iterable:
        if not predicate(x):
            break
        yield x


def filterfalse(predicate, iterable):
    if predicate is None:
        predicate = bool

    for x in iterable:
        if not predicate(x):
            yield x


def islice(iterable, *args):
    count_args = len(args)
    if count_args == 0 or count_args > 3:
        raise TypeError("islice expected at most 4 arguments, got " + str(count_args + 1))
    if count_args == 1:
        start = 0
        stop = args[0]
        step = 1
    else:
        start = 0 if args[0] is None else args[0]
        stop = args[1]
        step = 1 if count_args == 2 or args[2] is None else args[2]
    if start < 0 or (stop is not None and stop < 0) or step <= 0:
        raise ValueError(
            "Indices for islice() must be None or an integer: 0 <= x <= sys.maxsize."
        )

    indices = count() if stop is None else range(max(start, stop))
    next_i = start
    for i, element in zip(indices, iterable):
        if i == next_i:
            yield element
            next_i += step


def starmap(function, iterable):
    for args in iterable:
        yield function(*args)


def pairwise(iterable):
    iterator = iter(iterable)
    a = next(iterator, None)

    for b in iterator:
        yield a, b
        a = b


def batched(iterable, n, *, strict=False):
    if n < 1:
        raise ValueError("n must be at least one")
    iterator = iter(iterable)
    while True:
        batch = tuple(islice(iterator, n))
        if not batch:
            break
        if strict and len(batch) != n:
            raise ValueError("batched(): incomplete batch")
        yield batch


def zip_longest(*iterables, fillvalue=None):
    iterators = []
    for iterable in iterables:
        iterators.append(iter(iterable))
    num_active = len(iterators)
    if not num_active:
        return

    while True:
        values = []
        for i, iterator in enumerate(iterators):
            try:
                value = next(iterator)
            except StopIteration:
                num_active -= 1
                if not num_active:
                    return
                iterators[i] = repeat(fillvalue)
                value = fillvalue
            values.append(value)
        yield tuple(values)


def groupby(iterable, key=None):
    keyfunc = (lambda x: x) if key is None else key
    iterator = iter(iterable)
    exhausted = False
    curr_value = None
    curr_key = None

    def _grouper(target_key):
        nonlocal curr_value, curr_key, exhausted
        yield curr_value
        for curr_value in iterator:
            curr_key = keyfunc(curr_value)
            if curr_key != target_key:
                return
            yield curr_value
        exhausted = True

    for first in iterator:
        curr_value = first
        break
    else:
        return
    curr_key = keyfunc(curr_value)

    while not exhausted:
        target_key = curr_key
        curr_group = _grouper(target_key)
        yield curr_key, curr_group
        if curr_key == target_key:
            for _ in curr_group:
                pass


class _tee:
    def __init__(self, iterable):
        it = iter(iterable)
        if isinstance(it, _tee):
            self.iterator = it.iterator
            self.link = it.link
        else:
            self.iterator = it
            self.link = [None, None]

    def __iter__(self):
        return self

    def __next__(self):
        link = self.link
        if link[1] is None:
            link[0] = next(self.iterator)
            link[1] = [None, None]
        value = link[0]
        self.link = link[1]
        return value


def tee(iterable, n=2):
    if n < 0:
        raise ValueError("n must be >= 0")
    if n == 0:
        return ()
    iterator = _tee(iterable)
    result = [iterator]
    for _ in range(n - 1):
        result.append(_tee(iterator))
    return tuple(result)


def product(*iterables, repeat=1):
    if repeat < 0:
        raise ValueError("repeat argument cannot be negative")
    pools = []
    for _ in range(repeat):
        for pool in iterables:
            pools.append(tuple(pool))

    result = [[]]
    for pool in pools:
        result = [x + [y] for x in result for y in pool]

    for prod in result:
        yield tuple(prod)


def permutations(iterable, r=None):
    pool = tuple(iterable)
    n = len(pool)
    r = n if r is None else r
    if r > n:
        return

    indices = list(range(n))
    cycles = list(range(n, n - r, -1))
    yield tuple([pool[i] for i in indices[:r]])

    while n:
        for i in reversed(range(r)):
            cycles[i] -= 1
            if cycles[i] == 0:
                indices[i:] = indices[i + 1:] + indices[i:i + 1]
                cycles[i] = n - i
            else:
                j = cycles[i]
                indices[i], indices[-j] = indices[-j], indices[i]
                yield tuple([pool[k] for k in indices[:r]])
                break
        else:
            return


def combinations(iterable, r):
    pool = tuple(iterable)
    n = len(pool)
    if r > n:
        return
    indices = list(range(r))

    yield tuple([pool[i] for i in indices])
    while True:
        found = False
        for i in reversed(range(r)):
            if indices[i] != i + n - r:
                found = True
                break
        if not found:
            return
        indices[i] += 1
        for j in range(i + 1, r):
            indices[j] = indices[j - 1] + 1
        yield tuple([pool[k] for k in indices])


def combinations_with_replacement(iterable, r):
    pool = tuple(iterable)
    n = len(pool)
    if not n and r:
        return
    indices = [0] * r

    yield tuple([pool[i] for i in indices])
    while True:
        found = False
        for i in reversed(range(r)):
            if indices[i] != n - 1:
                found = True
                break
        if not found:
            return
        indices[i:] = [indices[i] + 1] * (r - i)
        yield tuple([pool[k] for k in indices])
