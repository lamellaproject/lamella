# The random module, bundled as a MANAGED module -- a pure-Python Mersenne Twister (MT19937) that
# reproduces CPython's `_random.Random` bit-for-bit, so a SEEDED sequence is identical to CPython's.
# The generator core (init_genrand / init_by_array / genrand_uint32 / random / getrandbits) is
# transcribed from CPython's `_randommodule.c`; the distribution helpers (randrange / randint /
# choice / shuffle / sample / uniform / _randbelow) from CPython's `Lib/random.py`. The differential
# therefore verifies this against the REAL (C-accelerated) module.
#
# SEED SUPPORT: an INT seed (or None) is reproducible. CPython also seeds from str/bytes (a SHA-512
# digest, version 2) and from a float/other (its hash); both need primitives we do not have yet
# (hashlib; the exact float hash), so a non-int, non-None seed raises a clear error rather than
# diverging silently. None is deterministic here (fixed fallback) where CPython uses system entropy --
# an unseeded generator is non-reproducible in CPython too, so this is unobservable to a differential.
#
# NOT bundled: getstate/setstate, the gauss/normal/etc. distribution family, and sample(counts=...).

# MT19937 constants (Matsumoto-Nishimura, as in _randommodule.c).
_N = 624
_M = 397
_MATRIX_A = 0x9908B0DF
_UPPER_MASK = 0x80000000
_LOWER_MASK = 0x7FFFFFFF
_MASK32 = 0xFFFFFFFF

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

    # ----- the generator core (transcribed from _randommodule.c) -----

    def _init_genrand(self, s):
        mt = [0] * _N
        mt[0] = s & _MASK32
        for i in range(1, _N):
            prev = mt[i - 1]
            mt[i] = (1812433253 * (prev ^ (prev >> 30)) + i) & _MASK32
        self.mt = mt
        self.mti = _N

    def _init_by_array(self, init_key):
        self._init_genrand(19650218)
        mt = self.mt
        key_length = len(init_key)
        i = 1
        j = 0
        # Arithmetic wraps mod 2**32; masking only the final store is exact, since +, -, *, ^ over the
        # low 32 bits do not depend on higher bits (carries propagate upward only).
        k = _N if _N > key_length else key_length
        while k:
            prev = mt[i - 1]
            mt[i] = ((mt[i] ^ ((prev ^ (prev >> 30)) * 1664525)) + init_key[j] + j) & _MASK32
            i += 1
            j += 1
            if i >= _N:
                mt[0] = mt[_N - 1]
                i = 1
            if j >= key_length:
                j = 0
            k -= 1
        k = _N - 1
        while k:
            prev = mt[i - 1]
            mt[i] = ((mt[i] ^ ((prev ^ (prev >> 30)) * 1566083941)) - i) & _MASK32
            i += 1
            if i >= _N:
                mt[0] = mt[_N - 1]
                i = 1
            k -= 1
        mt[0] = 0x80000000

    def _genrand_uint32(self):
        mt = self.mt
        if self.mti >= _N:
            for kk in range(_N - _M):
                y = (mt[kk] & _UPPER_MASK) | (mt[kk + 1] & _LOWER_MASK)
                mt[kk] = mt[kk + _M] ^ (y >> 1) ^ (_MATRIX_A if (y & 1) else 0)
            for kk in range(_N - _M, _N - 1):
                y = (mt[kk] & _UPPER_MASK) | (mt[kk + 1] & _LOWER_MASK)
                mt[kk] = mt[kk + (_M - _N)] ^ (y >> 1) ^ (_MATRIX_A if (y & 1) else 0)
            y = (mt[_N - 1] & _UPPER_MASK) | (mt[0] & _LOWER_MASK)
            mt[_N - 1] = mt[_M - 1] ^ (y >> 1) ^ (_MATRIX_A if (y & 1) else 0)
            self.mti = 0
        y = mt[self.mti]
        self.mti += 1
        # Tempering. Every intermediate stays within 32 bits (the AND-masks bound the shifts).
        y ^= y >> 11
        y ^= (y << 7) & 0x9D2C5680
        y ^= (y << 15) & 0xEFC60000
        y ^= y >> 18
        return y

    def random(self):
        # 53 bits of randomness: the top 27 and 26 bits of two words, as CPython's random_random.
        a = self._genrand_uint32() >> 5
        b = self._genrand_uint32() >> 6
        return (a * 67108864.0 + b) * _RECIP_53

    def getrandbits(self, k):
        if k < 0:
            raise ValueError("number of bits must be non-negative")
        if k == 0:
            return 0
        if k <= 32:
            return self._genrand_uint32() >> (32 - k)
        # Assemble little-endian 32-bit words; the most-significant word keeps only the leftover bits.
        words = (k - 1) // 32 + 1
        result = 0
        for i in range(words):
            r = self._genrand_uint32()
            if i == words - 1:
                r >>= 32 * words - k
            result |= r << (32 * i)
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
