"""Binary heaps kept in ordinary lists.

A min-heap keeps its smallest element at index 0; the _max functions keep
the largest there instead.  The children of index k sit at 2k+1 and 2k+2.
nsmallest and nlargest pick a few extremes out of any iterable without
sorting all of it.
"""

from _heapq import (heappush, heappop, heapreplace, heappushpop, heapify,
                    heappush_max, heappop_max, heapreplace_max, heappushpop_max,
                    heapify_max)

__all__ = ['heappush', 'heappop', 'heapify', 'heapreplace', 'heappushpop',
           'heappush_max', 'heappop_max', 'heapify_max', 'heapreplace_max',
           'heappushpop_max', 'nlargest', 'nsmallest']

_heappush_max = heappush_max
_heappop_max = heappop_max
_heapreplace_max = heapreplace_max
_heappushpop_max = heappushpop_max
_heapify_max = heapify_max

_NOTHING = object()


def nsmallest(n, iterable, key=None):
    """Return a new list of the n smallest elements of iterable, ascending, with
    equal elements in their input order: sorted(iterable, key=key)[:n], found
    with a bounded heap instead of a full sort."""
    if n == 1:
        found = min(iterable, default=_NOTHING, key=key)
        if found is _NOTHING:
            return []
        return [found]
    try:
        size = len(iterable)
    except (TypeError, AttributeError):
        pass
    else:
        if n >= size:
            return sorted(iterable, key=key)[:n]
    it = iter(iterable)
    # Entries pair each value with its arrival number, so equal values order by
    # arrival and the elements themselves are never compared.
    if key is None:
        entries = [(elem, i) for i, elem in zip(range(n), it)]
        if not entries:
            return entries
        heapify_max(entries)
        top = entries[0][0]
        order = n
        for elem in it:
            if elem < top:
                heapreplace_max(entries, (elem, order))
                top = entries[0][0]
                order += 1
        entries.sort()
        return [entry[0] for entry in entries]
    entries = [(key(elem), i, elem) for i, elem in zip(range(n), it)]
    if not entries:
        return entries
    heapify_max(entries)
    top = entries[0][0]
    order = n
    for elem in it:
        k = key(elem)
        if k < top:
            heapreplace_max(entries, (k, order, elem))
            top = entries[0][0]
            order += 1
    entries.sort()
    return [entry[2] for entry in entries]


def nlargest(n, iterable, key=None):
    """Return a new list of the n largest elements of iterable, descending, with
    equal elements in their input order: sorted(iterable, key=key,
    reverse=True)[:n], found with a bounded heap instead of a full sort."""
    if n == 1:
        found = max(iterable, default=_NOTHING, key=key)
        if found is _NOTHING:
            return []
        return [found]
    try:
        size = len(iterable)
    except (TypeError, AttributeError):
        pass
    else:
        if n >= size:
            return sorted(iterable, key=key, reverse=True)[:n]
    it = iter(iterable)
    # Arrival numbers count downward here, so that among equal values the
    # earliest arrival is the largest entry and survives in the min-heap.
    if key is None:
        entries = [(elem, i) for i, elem in zip(range(0, -n, -1), it)]
        if not entries:
            return entries
        heapify(entries)
        top = entries[0][0]
        order = -n
        for elem in it:
            if top < elem:
                heapreplace(entries, (elem, order))
                top = entries[0][0]
                order -= 1
        entries.sort(reverse=True)
        return [entry[0] for entry in entries]
    entries = [(key(elem), i, elem) for i, elem in zip(range(0, -n, -1), it)]
    if not entries:
        return entries
    heapify(entries)
    top = entries[0][0]
    order = -n
    for elem in it:
        k = key(elem)
        if top < k:
            heapreplace(entries, (k, order, elem))
            top = entries[0][0]
            order -= 1
    entries.sort(reverse=True)
    return [entry[2] for entry in entries]
