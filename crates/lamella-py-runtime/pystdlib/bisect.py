# The pure-Python bisect module, bundled as a MANAGED module (CPython accelerates it with the C
# _bisect extension, which we have no equivalent for; CPython's own pure-Python fallback -- transcribed
# here -- is behaviorally identical, so the differential verifies this against CPython's real bisect).
# Bisection algorithms over a sorted list, with an optional `key` and `lo`/`hi` bounds.


def bisect_right(a, x, lo=0, hi=None, *, key=None):
    # The index i such that every e in a[:i] has e <= x and every e in a[i:] has e > x -- so
    # a.insert(i, x) lands just after the rightmost existing x. The comparison uses "<" to match
    # list.sort()/heapq's __lt__ logic.
    if lo < 0:
        raise ValueError("lo must be non-negative")
    if hi is None:
        hi = len(a)
    if key is None:
        while lo < hi:
            mid = (lo + hi) // 2
            if x < a[mid]:
                hi = mid
            else:
                lo = mid + 1
    else:
        while lo < hi:
            mid = (lo + hi) // 2
            if x < key(a[mid]):
                hi = mid
            else:
                lo = mid + 1
    return lo


def bisect_left(a, x, lo=0, hi=None, *, key=None):
    # The index i such that every e in a[:i] has e < x and every e in a[i:] has e >= x -- so
    # a.insert(i, x) lands just before the leftmost existing x.
    if lo < 0:
        raise ValueError("lo must be non-negative")
    if hi is None:
        hi = len(a)
    if key is None:
        while lo < hi:
            mid = (lo + hi) // 2
            if a[mid] < x:
                lo = mid + 1
            else:
                hi = mid
    else:
        while lo < hi:
            mid = (lo + hi) // 2
            if key(a[mid]) < x:
                lo = mid + 1
            else:
                hi = mid
    return lo


def insort_right(a, x, lo=0, hi=None, *, key=None):
    # Insert x into the sorted list a, to the right of any equal items.
    if key is None:
        lo = bisect_right(a, x, lo, hi)
    else:
        lo = bisect_right(a, key(x), lo, hi, key=key)
    a.insert(lo, x)


def insort_left(a, x, lo=0, hi=None, *, key=None):
    # Insert x into the sorted list a, to the left of any equal items.
    if key is None:
        lo = bisect_left(a, x, lo, hi)
    else:
        lo = bisect_left(a, key(x), lo, hi, key=key)
    a.insert(lo, x)


# The historical short names alias the *_right forms.
bisect = bisect_right
insort = insort_right
