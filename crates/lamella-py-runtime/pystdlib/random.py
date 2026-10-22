# The random module, bundled as a managed module -- a pure-Python Mersenne Twister (MT19937) that
# reproduces CPython's `_random.Random` bit-for-bit, so a seeded sequence is identical to CPython's.
#
# The generator core -- the recurrence, the tempering, both seeding procedures and the outputs built
# on them -- implements the MT19937 algorithm of Makoto Matsumoto and Takuji Nishimura (1998, with the
# initialization they revised in 2002), who are credited for it. The array seeding is their 2002
# scheme, the one CPython's integer seeding uses, which is what keeps a seeded stream identical to
# CPython's.
#
# The helpers built on the generator (randrange / randint / choice / shuffle / sample / uniform)
# reproduce CPython's output for a given seed exactly: the same values, the same orders, the same
# number of draws per call, and the same errors.
#
# SEED SUPPORT: an int seed (or None) is reproducible. CPython also seeds from str/bytes (a SHA-512
# digest, version 2) and from a float/other (its hash); both need primitives this runtime does not
# provide (hashlib; the exact float hash), so a non-int, non-None seed raises a clear error rather than
# diverging silently. None is deterministic here (fixed fallback) where CPython uses system entropy --
# an unseeded generator is non-reproducible in CPython too, so this is unobservable to a differential.
#
# Not bundled: getstate/setstate, the gauss/normal/etc. distribution family, and sample(counts=...).

# MT19937's published parameters (Matsumoto and Nishimura, 1998).
_N = 624                        # words of state
_M = 397                        # offset of the word mixed into each new one
_MATRIX_A = 0x9908B0DF          # the twist, applied when the shifted value was odd
_UPPER_MASK = 0x80000000        # the single high bit taken from a word
_LOWER_MASK = 0x7FFFFFFF        # the 31 low bits taken from the word after it
_MASK32 = 0xFFFFFFFF

# Seeding constants: the single-word multiplier, then the 2002 array scheme's.
_SEED_MULTIPLIER = 1812433253
_ARRAY_BASE_SEED = 19650218
_ARRAY_MULT_1 = 1664525         # first pass, which absorbs the key
_ARRAY_MULT_2 = 1566083941      # second pass, which diffuses it
_ARRAY_FINAL_WORD = 0x80000000  # written into word 0 last, so the state is never all zero

# The reciprocal 2**-53 and 2**26, spelled as CPython's random_random does, so the double is identical.
_RECIP_53 = 1.0 / 9007199254740992.0

# The sequence types sample() accepts, with collections.deque, which it imports only when it needs it.
# CPython accepts any registered Sequence; with no collections.abc here, the built-in ones are named.
_SEQUENCE_TYPES = (list, tuple, range, str, bytes, bytearray, memoryview)


# An integer argument, converted as operator.index converts it: an int, including a bool or another
# subclass of int, gives its own value without its type's __index__ being called; anything else goes
# through its type's __index__. It is written out here because importing the operator module costs
# object heap.
def _as_int(value):
    if issubclass(type(value), int):
        return int.__index__(value)
    try:
        convert = type(value).__index__
    except AttributeError:
        name = type(value).__name__
        raise TypeError("'{}' object cannot be interpreted as an integer".format(name)) from None
    result = convert(value)
    if type(result) is not int:
        if not issubclass(type(result), int):
            raise TypeError("__index__ returned non-int (type {})".format(type(result).__name__))
        result = int.__index__(result)
    return result


# The Mersenne Twister generator. Holds the 624-word state `mt` and the read index `mti`; every
# public draw consumes 32-bit words through _genrand_uint32, exactly as CPython's C core does.
class Random:
    def __init__(self, x=None):
        self.gauss_next = None
        self.seed(x)

    # ----- the generator core (ours -- see the file header for its provenance) -----

    def _init_genrand(self, s):
        # Fill the state from one word. Each word folds the top two bits of the one before it down
        # before multiplying, then adds its own index.
        mt = [0] * _N
        mt[0] = s & _MASK32
        for i in range(1, _N):
            prev = mt[i - 1]
            mt[i] = (_SEED_MULTIPLIER * (prev ^ (prev >> 30)) + i) & _MASK32
        self.mt = mt
        self.mti = _N

    def _init_by_array(self, key):
        # Fill the state from a key of one or more words, in four steps: build a starting state from
        # a fixed seed; walk positions 1..n-1 cyclically for max(n, len(key)) steps absorbing the key;
        # walk n-1 further steps diffusing, continuing round the same ring; then fix word 0. Whenever
        # the last position is written, its value is copied into word 0, so position 1 always reads
        # the current last word.
        #
        # The state is modified in place: a second 624-word list would double what seeding costs
        # the object heap.
        if not key:
            raise ValueError("the seed key must contain at least one word")
        self._init_genrand(_ARRAY_BASE_SEED)
        mt = self.mt
        ring = _N - 1
        length = len(key)
        first_pass = _N if _N > length else length
        for t in range(first_pass):
            p = 1 + t % ring
            q = t % length
            prev = mt[p - 1]
            product = (_ARRAY_MULT_1 * (prev ^ (prev >> 30))) & _MASK32
            mt[p] = ((mt[p] ^ product) + (key[q] & _MASK32) + q) & _MASK32
            if p == ring:
                mt[0] = mt[ring]
        for j in range(ring):
            p = 1 + (first_pass + j) % ring
            prev = mt[p - 1]
            product = (_ARRAY_MULT_2 * (prev ^ (prev >> 30))) & _MASK32
            mt[p] = ((mt[p] ^ product) - p) & _MASK32
            if p == ring:
                mt[0] = mt[ring]
        mt[0] = _ARRAY_FINAL_WORD

    def _regenerate(self):
        # Overwrite every word with the next one, in increasing order. Split into three ranges so no
        # index needs a modulo: the second range wraps the m-offset, and the last word wraps both.
        mt = self.mt
        for k in range(_N - _M):
            y = (mt[k] & _UPPER_MASK) | (mt[k + 1] & _LOWER_MASK)
            mt[k] = mt[k + _M] ^ (y >> 1) ^ (_MATRIX_A if y & 1 else 0)
        for k in range(_N - _M, _N - 1):
            y = (mt[k] & _UPPER_MASK) | (mt[k + 1] & _LOWER_MASK)
            mt[k] = mt[k + _M - _N] ^ (y >> 1) ^ (_MATRIX_A if y & 1 else 0)
        y = (mt[_N - 1] & _UPPER_MASK) | (mt[0] & _LOWER_MASK)
        mt[_N - 1] = mt[_M - 1] ^ (y >> 1) ^ (_MATRIX_A if y & 1 else 0)
        self.mti = 0

    def _genrand_uint32(self):
        # The next output: take a word from the block, regenerating first if the block is spent, then
        # temper it. Tempering is what makes the raw recurrence equidistributed.
        mt = self.mt
        if self.mti >= _N:
            self._regenerate()
        y = mt[self.mti]
        self.mti += 1
        y ^= y >> 11
        y ^= (y << 7) & 0x9D2C5680
        y ^= (y << 15) & 0xEFC60000
        y ^= y >> 18
        return y

    def random(self):
        # 53 random bits: the leading 27 of one output above the leading 26 of the next. Kept in the
        # float domain because the combined integer exceeds the fixnum bound and would put a heap
        # value on the hottest path in the module; the double is identical either way.
        high = self._genrand_uint32() >> 5
        low = self._genrand_uint32() >> 6
        return (high * 67108864.0 + low) * _RECIP_53

    def getrandbits(self, k):
        # k random bits, filled from the least significant end 32 at a time; a final partial chunk
        # takes the leading bits of its word. k == 0 consumes no words.
        if k < 0:
            raise ValueError("number of bits must be non-negative")
        result = 0
        position = 0
        while k > 0:
            word = self._genrand_uint32()
            if k < 32:
                word >>= 32 - k
            result |= word << position
            position += 32
            k -= 32
        return result

    # ----- seeding (the int path of _randommodule.c's random_seed) -----

    def seed(self, a=None, version=2):
        if a is None:
            # CPython seeds from os.urandom here (non-reproducible); we use a fixed fallback so an
            # unseeded generator is at least deterministic. Reseed explicitly with an int to reproduce.
            a = 0
        elif isinstance(a, bool) or not isinstance(a, int):
            raise TypeError(
                "this build reproduces int (and None) seeds only; str/bytes seeding needs a "
                "SHA-512 we do not carry yet, and float/other seeding needs the exact object hash"
            )
        # Split abs(a) into little-endian 32-bit words -- exactly CPython's keymax = ceil(bits/32)
        # words (min one), so init_by_array sees the identical key.
        n = a if a >= 0 else -a
        key = []
        while n:
            key.append(n & _MASK32)
            n >>= 32
        if not key:
            key = [0]
        self._init_by_array(key)
        self.gauss_next = None

    def __copy__(self):
        # A generator's state is the point of it, so a copy gets its OWN state vector: two generators
        # sharing one would advance together, and a copy that advances the original is not a copy.
        made = Random(0)
        made.mt = list(self.mt)
        made.mti = self.mti
        made.gauss_next = self.gauss_next
        return made

    def __deepcopy__(self, memo):
        return self.__copy__()

    # ----- the helpers built on the generator -----

    # A uniform int in [0, n), for n >= 1: draw as many bits as n has, and draw again whenever the
    # value lands at or above n. Even n == 1 draws.
    def _below(self, n):
        bits = n.bit_length()
        while True:
            value = self.getrandbits(bits)
            if value < n:
                return value

    # A random int from range(start, stop, step); randrange(n) draws from range(n). Without stop, step
    # must be left at the int 1. One draw sequence per call, after every check.
    def randrange(self, start, stop=None, step=1):
        first = start if type(start) is int else _as_int(start)
        if stop is None:
            if type(step) is not int or step != 1:
                raise TypeError("Missing a non-None stop argument")
            if first > 0:
                return self._below(first)
            raise ValueError("empty range for randrange()")
        span = (stop if type(stop) is int else _as_int(stop)) - first
        pace = step if type(step) is int else _as_int(step)
        if pace == 1:
            if span > 0:
                return first + self._below(span)
            raise ValueError("empty range in randrange({}, {})".format(start, stop))
        if not pace:
            raise ValueError("zero step for randrange()")
        # How many values the range holds: span / pace, rounded up.
        count = -(-span // pace)
        if count > 0:
            return first + pace * self._below(count)
        raise ValueError("empty range in randrange({}, {}, {})".format(start, stop, step))

    # A random int from a to b, both ends included.
    def randint(self, a, b):
        if type(a) is not int:
            a = _as_int(a)
        if type(b) is not int:
            b = _as_int(b)
        if b < a:
            raise ValueError("empty range in randint({}, {})".format(a, b))
        return a + self._below(b - a + 1)

    # A random item of a non-empty sequence, fetched by its index.
    def choice(self, seq):
        size = len(seq)
        if not size:
            raise IndexError("Cannot choose from an empty sequence")
        return seq[self._below(size)]

    # Permutes x in place: from the last position down to the second, each position trades items with
    # one drawn at or below it (Durstenfeld's form of the Fisher-Yates shuffle).
    def shuffle(self, x):
        for p in range(len(x) - 1, 0, -1):
            q = self._below(p + 1)
            x[p], x[q] = x[q], x[p]

    # k distinct positions of population, as a new list in the order they were drawn. A population
    # small enough beside k is copied, and each pick fills its gap with the last item still in play;
    # a larger one is read in place, with the positions already taken remembered.
    def sample(self, population, k):
        if not isinstance(population, _SEQUENCE_TYPES):
            from collections import deque
            if not isinstance(population, deque):
                raise TypeError("Population must be a sequence.  For dicts or sets, use sorted(d).")
        n = len(population)
        if not 0 <= k <= n:
            raise ValueError("Sample larger than population or is negative")
        result = [None] * k
        # The copy limit: 21, plus the least power of four at or above 3k once k exceeds 5.
        limit = 21
        if k > 5:
            triple = k * 3
            power = 4
            while power < triple:
                power *= 4
            limit += power
        if n <= limit:
            pool = list(population)
            live = n
            for i in range(k):
                j = self._below(live)
                result[i] = pool[j]
                live -= 1
                pool[j] = pool[live]
        else:
            taken = {}
            for i in range(k):
                while True:
                    j = self._below(n)
                    if j not in taken:
                        break
                taken[j] = True
                result[i] = population[j]
        return result

    # A random float between a and b: a + (b - a) * random(), the difference taken before the draw.
    def uniform(self, a, b):
        return a + (b - a) * self.random()


# The module-level facade delegates to one shared instance, exactly as CPython's random module binds
# `random = _inst.random`, `seed = _inst.seed`, and so on.
_inst = Random()
seed = _inst.seed
random = _inst.random
uniform = _inst.uniform
randrange = _inst.randrange
randint = _inst.randint
choice = _inst.choice
shuffle = _inst.shuffle
sample = _inst.sample
getrandbits = _inst.getrandbits
