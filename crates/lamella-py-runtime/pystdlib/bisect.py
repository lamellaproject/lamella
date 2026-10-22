"""Binary search for insertion points in sorted sequences, and sorted insertion.

bisect_left and bisect_right find where x belongs in the sorted sequence a,
before or after any entries equal to x; insort_left and insort_right insert it
there.  bisect and insort are the "right" variants.  Only the < operator is
used, on key(element) when a key function is given, so a never needs __eq__.
"""

from _bisect import bisect_left, bisect_right, insort_left, insort_right

bisect = bisect_right
insort = insort_right
