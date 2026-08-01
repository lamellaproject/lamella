# The pure-Python heapq module, bundled as a MANAGED module (CPython accelerates it with the C _heapq
# extension; its own pure-Python fallback -- transcribed here -- is behaviorally identical, so the
# differential verifies this against CPython's real heapq). A binary min-heap kept in a list: the
# invariant is heap[k] <= heap[2*k+1] and heap[k] <= heap[2*k+2]. `merge` is not bundled; everything
# else (push/pop/replace/pushpop/heapify, the _max variants, nsmallest/nlargest) is.


def heappush(heap, item):
    # Push item onto heap, maintaining the invariant.
    heap.append(item)
    _siftdown(heap, 0, len(heap) - 1)


def heappop(heap):
    # Pop the smallest item off the heap, maintaining the invariant.
    lastelt = heap.pop()  # raises IndexError if empty
    if heap:
        returnitem = heap[0]
        heap[0] = lastelt
        _siftup(heap, 0)
        return returnitem
    return lastelt


def heapreplace(heap, item):
    # Pop and return the smallest, and add item -- one sift, not two. The returned value may be
    # larger than item.
    returnitem = heap[0]  # raises IndexError if empty
    heap[0] = item
    _siftup(heap, 0)
    return returnitem


def heappushpop(heap, item):
    # A push followed by a pop, faster as one operation.
    if heap and heap[0] < item:
        item, heap[0] = heap[0], item
        _siftup(heap, 0)
    return item


def heapify(x):
    # Transform list into a heap in-place, in O(len(x)).
    n = len(x)
    for i in reversed(range(n // 2)):
        _siftup(x, i)


def _siftdown(heap, startpos, pos):
    newitem = heap[pos]
    # Follow the path to the root, moving parents down until newitem fits.
    while pos > startpos:
        parentpos = (pos - 1) >> 1
        parent = heap[parentpos]
        if newitem < parent:
            heap[pos] = parent
            pos = parentpos
            continue
        break
    heap[pos] = newitem


def _siftup(heap, pos):
    endpos = len(heap)
    startpos = pos
    newitem = heap[pos]
    # Bubble the smaller child up until hitting a leaf, then sift newitem down into place.
    childpos = 2 * pos + 1
    while childpos < endpos:
        rightpos = childpos + 1
        if rightpos < endpos and not heap[childpos] < heap[rightpos]:
            childpos = rightpos
        heap[pos] = heap[childpos]
        pos = childpos
        childpos = 2 * pos + 1
    heap[pos] = newitem
    _siftdown(heap, startpos, pos)


def heappush_max(heap, item):
    # Maxheap version of heappush.
    heap.append(item)
    _siftdown_max(heap, 0, len(heap) - 1)


def heappop_max(heap):
    # Maxheap version of heappop.
    lastelt = heap.pop()
    if heap:
        returnitem = heap[0]
        heap[0] = lastelt
        _siftup_max(heap, 0)
        return returnitem
    return lastelt


def heapreplace_max(heap, item):
    # Maxheap version of heapreplace.
    returnitem = heap[0]
    heap[0] = item
    _siftup_max(heap, 0)
    return returnitem


def heappushpop_max(heap, item):
    # Maxheap version of heappushpop.
    if heap and item < heap[0]:
        item, heap[0] = heap[0], item
        _siftup_max(heap, 0)
    return item


def heapify_max(x):
    # Transform list into a maxheap in-place.
    n = len(x)
    for i in reversed(range(n // 2)):
        _siftup_max(x, i)


def _siftdown_max(heap, startpos, pos):
    newitem = heap[pos]
    while pos > startpos:
        parentpos = (pos - 1) >> 1
        parent = heap[parentpos]
        if parent < newitem:
            heap[pos] = parent
            pos = parentpos
            continue
        break
    heap[pos] = newitem


def _siftup_max(heap, pos):
    endpos = len(heap)
    startpos = pos
    newitem = heap[pos]
    childpos = 2 * pos + 1
    while childpos < endpos:
        rightpos = childpos + 1
        if rightpos < endpos and not heap[rightpos] < heap[childpos]:
            childpos = rightpos
        heap[pos] = heap[childpos]
        pos = childpos
        childpos = 2 * pos + 1
    heap[pos] = newitem
    _siftdown_max(heap, startpos, pos)


# A unique module-private sentinel for the n==1 min/max shortcut (CPython uses a fresh object();
# `object()` is out of our builtin subset, so a private class instance serves the same identity role).
class _Sentinel:
    pass


_MISSING = _Sentinel()


def nsmallest(n, iterable, key=None):
    # The n smallest elements, like sorted(iterable, key=key)[:n] but cheaper for small n.
    if n == 1:
        it = iter(iterable)
        result = min(it, default=_MISSING, key=key)
        return [] if result is _MISSING else [result]
    try:
        size = len(iterable)
    except (TypeError, AttributeError):
        pass
    else:
        if n >= size:
            return sorted(iterable, key=key)[:n]
    if key is None:
        it = iter(iterable)
        result = [(elem, i) for i, elem in zip(range(n), it)]
        if not result:
            return result
        heapify_max(result)
        top = result[0][0]
        order = n
        _heapreplace = heapreplace_max
        for elem in it:
            if elem < top:
                _heapreplace(result, (elem, order))
                top, _order = result[0]
                order += 1
        result.sort()
        return [elem for elem, order in result]
    it = iter(iterable)
    result = [(key(elem), i, elem) for i, elem in zip(range(n), it)]
    if not result:
        return result
    heapify_max(result)
    top = result[0][0]
    order = n
    _heapreplace = heapreplace_max
    for elem in it:
        k = key(elem)
        if k < top:
            _heapreplace(result, (k, order, elem))
            top, _order, _elem = result[0]
            order += 1
    result.sort()
    return [elem for k, order, elem in result]


def nlargest(n, iterable, key=None):
    # The n largest elements, like sorted(iterable, key=key, reverse=True)[:n] but cheaper for small n.
    if n == 1:
        it = iter(iterable)
        result = max(it, default=_MISSING, key=key)
        return [] if result is _MISSING else [result]
    try:
        size = len(iterable)
    except (TypeError, AttributeError):
        pass
    else:
        if n >= size:
            return sorted(iterable, key=key, reverse=True)[:n]
    if key is None:
        it = iter(iterable)
        result = [(elem, i) for i, elem in zip(range(0, -n, -1), it)]
        if not result:
            return result
        heapify(result)
        top = result[0][0]
        order = -n
        _heapreplace = heapreplace
        for elem in it:
            if top < elem:
                _heapreplace(result, (elem, order))
                top, _order = result[0]
                order -= 1
        result.sort(reverse=True)
        return [elem for elem, order in result]
    it = iter(iterable)
    result = [(key(elem), i, elem) for i, elem in zip(range(0, -n, -1), it)]
    if not result:
        return result
    heapify(result)
    top = result[0][0]
    order = -n
    _heapreplace = heapreplace
    for elem in it:
        k = key(elem)
        if top < k:
            _heapreplace(result, (k, order, elem))
            top, _order, _elem = result[0]
            order -= 1
    result.sort(reverse=True)
    return [elem for k, order, elem in result]
