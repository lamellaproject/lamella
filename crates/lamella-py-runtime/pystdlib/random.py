# The random module, bundled as a managed module -- a pure-Python Mersenne Twister (MT19937) that
# reproduces CPython's `_random.Random` bit-for-bit, so a seeded sequence is identical to CPython's.
#
# The generator core -- the recurrence, the tempering, both seeding procedures and the outputs built
# on them -- is written clean-room from a written specification of the algorithm. The algorithm is
# Makoto Matsumoto and Takuji Nishimura's (1998, with the initialization they revised in 2002), and
# they are credited for it. The array seeding is their 2002 scheme, the one CPython's integer seeding
# uses, which is what keeps a seeded stream identical to CPython's.
#
# The distribution helpers (randrange / randint / choice / shuffle / sample / uniform / _randbelow)
# are derived from CPython's `Lib/random.py`, so CPython's license applies to this file; it is in
# LICENSE-CPYTHON beside it.
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

# The known-sequence types sample() accepts (CPython requires a Sequence; a set/dict/generator is a
# TypeError there since 3.11). We name the concrete builtins rather than an abc we do not carry.
_SEQUENCE_TYPES = (list, tuple, range, str, bytes, bytearray)


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

    # ----- distributions (transcribed from Lib/random.py) -----

    def _randbelow(self, n):
        # Return a random int in [0, n) using getrandbits, with rejection so the result is unbiased
        # and consumes words exactly as CPython does.
        if not n:
            return 0
        k = n.bit_length()
        r = self.getrandbits(k)
        while r >= n:
            r = self.getrandbits(k)
        return r

    def randrange(self, start, stop=None, step=1):
        # Integer-valued arguments (CPython uses operator.index); the arithmetic and the single
        # _randbelow draw match CPython so the stream stays aligned.
        istart = start
        if stop is None:
            if istart > 0:
                return self._randbelow(istart)
            raise ValueError("empty range for randrange()")
        istop = stop
        width = istop - istart
        istep = step
        if istep == 1:
            if width > 0:
                return istart + self._randbelow(width)
            raise ValueError("empty range in randrange(%d, %d)" % (istart, istop))
        if istep > 0:
            n = (width + istep - 1) // istep
        elif istep < 0:
            n = (width + istep + 1) // istep
        else:
            raise ValueError("zero step for randrange()")
        if n <= 0:
            raise ValueError("empty range for randrange()")
        return istart + istep * self._randbelow(n)

    def randint(self, a, b):
        # Inclusive of both endpoints: randrange(a, b + 1).
        return self.randrange(a, b + 1)

    def choice(self, seq):
        if not len(seq):
            raise IndexError("Cannot choose from an empty sequence")
        return seq[self._randbelow(len(seq))]

    def shuffle(self, x):
        # Fisher-Yates, from the high end -- the exact draw order of CPython's shuffle.
        randbelow = self._randbelow
        for i in range(len(x) - 1, 0, -1):
            j = randbelow(i + 1)
            x[i], x[j] = x[j], x[i]

    def sample(self, population, k):
        if not isinstance(population, _SEQUENCE_TYPES):
            raise TypeError("Population must be a sequence.  For dicts or sets, use sorted(d).")
        n = len(population)
        if not 0 <= k <= n:
            raise ValueError("Sample larger than population or is negative")
        randbelow = self._randbelow
        result = [None] * k
        # setsize mirrors CPython's `21 + 4 ** ceil(log(k * 3, 4))` for k > 5, but computed in exact
        # integers: 4 ** ceil(log(t, 4)) is the least power of 4 >= t, and t = k*3 is never itself a
        # power of 4 (4**m is not divisible by 3), so there is no float tie point to disagree on.
        setsize = 21
        if k > 5:
            target = k * 3
            power = 1
            while power < target:
                power *= 4
            setsize += power
        if n <= setsize:
            # An n-length pool is cheaper than a k-length set: swap chosen items out of the pool.
            pool = list(population)
            for i in range(k):
                j = randbelow(n - i)
                result[i] = pool[j]
                pool[j] = pool[n - i - 1]
        else:
            selected = set()
            selected_add = selected.add
            for i in range(k):
                j = randbelow(n)
                while j in selected:
                    j = randbelow(n)
                selected_add(j)
                result[i] = population[j]
        return result

    def uniform(self, a, b):
        # a <= N <= b (or b <= N <= a); the same float ops as CPython, so N is bit-identical.
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
