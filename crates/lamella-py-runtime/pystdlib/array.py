# array, bundled as a MANAGED module: a compact sequence of one numeric type, stored as bytes.
#
# The storage is a `bytearray` and the conversion is `struct`'s NATIVE mode, which is what makes the
# item sizes right without a second table: `array('l')` holds 4 bytes per item where a C `long` is 4
# and 8 where it is 8, because that is the size struct derived from the compiler. A single item has
# nothing before it to align against, so native packing here is native WIDTH with no padding.
#
# Two behaviours worth knowing, both matching the standard module: equality compares VALUES and not
# typecodes, so `array('h', [1, 2]) == array('i', [1, 2])`; and a slice of an array is an array.
import struct

_TYPECODES = "bBhHiIlLqQfd"
_INTEGER_CODES = "bBhHiIlLqQ"

# The standard module's own complaints, which are per-typecode and not uniform. A caller who hits one
# is usually about to change a typecode, so saying which limit was crossed is the useful part.
_LIMIT_MESSAGES = {
    "b": ["signed char is greater than maximum", "signed char is less than minimum"],
    "B": ["unsigned byte integer is greater than maximum", "unsigned byte integer is less than minimum"],
    "h": ["signed short integer is greater than maximum", "signed short integer is less than minimum"],
    "H": ["unsigned short is greater than maximum", "unsigned short is less than minimum"],
    "i": ["Python int too large to convert to C long", "Python int too large to convert to C long"],
    "I": ["Python int too large to convert to C unsigned long", "can't convert negative value to unsigned int"],
    "l": ["Python int too large to convert to C long", "Python int too large to convert to C long"],
    "L": ["Python int too large to convert to C unsigned long", "can't convert negative value to unsigned int"],
    "q": ["int too big to convert", "int too big to convert"],
    "Q": ["int too big to convert", "can't convert negative int to unsigned"],
}


def _check_typecode(typecode):
    if not isinstance(typecode, str) or len(typecode) != 1:
        raise TypeError("array() argument 1 must be a unicode character, not " + type(typecode).__name__)
    if typecode == "u" or typecode == "w":
        raise ValueError("the text typecodes 'u' and 'w' are not supported")
    if typecode not in _TYPECODES:
        raise ValueError("bad typecode (must be b, B, u, w, h, H, i, I, l, L, q, Q, f or d)")


class array:
    def __init__(self, typecode, initializer=None):
        _check_typecode(typecode)
        self.typecode = typecode
        self._format = "@" + typecode
        self.itemsize = struct.calcsize(self._format)
        self._buffer = bytearray()
        if initializer is not None:
            self.extend(initializer)

    # --- conversion of one item, and the range check the standard module performs ---

    def _pack(self, value):
        if self.typecode in _INTEGER_CODES:
            if isinstance(value, bool):
                value = int(value)
            if not isinstance(value, int):
                raise TypeError(
                    "'" + type(value).__name__ + "' object cannot be interpreted as an integer"
                )
            bits = 8 * self.itemsize
            if self.typecode in "bhilq":
                low = -(1 << (bits - 1))
                high = (1 << (bits - 1)) - 1
            else:
                low = 0
                high = (1 << bits) - 1
            if value > high:
                raise OverflowError(_LIMIT_MESSAGES[self.typecode][0])
            if value < low:
                raise OverflowError(_LIMIT_MESSAGES[self.typecode][1])
        else:
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                raise TypeError("must be real number, not " + type(value).__name__)
            value = float(value)
        return struct.pack(self._format, value)

    def _item(self, index):
        start = index * self.itemsize
        return struct.unpack(self._format, bytes(self._buffer[start:start + self.itemsize]))[0]

    def _index(self, index):
        count = len(self)
        if index < 0:
            index = index + count
        if index < 0 or index >= count:
            raise IndexError("array index out of range")
        return index

    # --- the sequence protocol ---

    def __len__(self):
        return len(self._buffer) // self.itemsize

    def __getitem__(self, index):
        if isinstance(index, slice):
            out = array(self.typecode)
            for i in range(*index.indices(len(self))):
                out.append(self._item(i))
            return out
        return self._item(self._index(index))

    def __setitem__(self, index, value):
        index = self._index(index)
        start = index * self.itemsize
        self._buffer[start:start + self.itemsize] = self._pack(value)

    def __delitem__(self, index):
        index = self._index(index)
        start = index * self.itemsize
        del self._buffer[start:start + self.itemsize]

    def __iter__(self):
        for i in range(len(self)):
            yield self._item(i)

    def __contains__(self, value):
        for item in self:
            if item == value:
                return True
        return False

    def __eq__(self, other):
        # Values, not typecodes -- the standard module compares an array of shorts equal to an array
        # of ints holding the same numbers.
        if not isinstance(other, array):
            return NotImplemented
        if len(self) != len(other):
            return False
        for i in range(len(self)):
            if self._item(i) != other._item(i):
                return False
        return True

    def __ne__(self, other):
        result = self.__eq__(other)
        if result is NotImplemented:
            return result
        return not result

    def __add__(self, other):
        if not isinstance(other, array):
            raise TypeError("can only append array (not \"" + type(other).__name__ + "\") to array")
        if other.typecode != self.typecode:
            raise TypeError("bad argument type for built-in operation")
        out = array(self.typecode)
        out._buffer = bytearray(self._buffer) + bytearray(other._buffer)
        return out

    def __mul__(self, count):
        if not isinstance(count, int):
            raise TypeError("can't multiply sequence by non-int of type '" + type(count).__name__ + "'")
        out = array(self.typecode)
        if count > 0:
            out._buffer = bytearray(self._buffer) * count
        return out

    def __rmul__(self, count):
        return self.__mul__(count)

    def __repr__(self):
        if len(self) == 0:
            return "array('" + self.typecode + "')"
        return "array('" + self.typecode + "', " + repr(list(self)) + ")"

    def __copy__(self):
        # An array's items ARE its state, so even a shallow copy has to reproduce the storage: sharing
        # the buffer would make writing to the copy write to the original.
        made = array(self.typecode)
        made._buffer = bytearray(self._buffer)
        return made

    def __deepcopy__(self, memo):
        # Items are numbers, so there is nothing deeper to copy than the storage itself.
        return self.__copy__()

    # --- the mutating verbs ---

    def append(self, value):
        self._buffer = self._buffer + bytearray(self._pack(value))

    def extend(self, values):
        if isinstance(values, array):
            if values.typecode != self.typecode:
                raise TypeError("can only extend with array of same kind")
            self._buffer = self._buffer + bytearray(values._buffer)
            return
        for value in values:
            self.append(value)

    def insert(self, index, value):
        count = len(self)
        if index < 0:
            index = index + count
            if index < 0:
                index = 0
        if index > count:
            index = count
        start = index * self.itemsize
        self._buffer[start:start] = self._pack(value)

    def pop(self, index=-1):
        if len(self) == 0:
            raise IndexError("pop from empty array")
        index = self._index(index)
        value = self._item(index)
        self.__delitem__(index)
        return value

    def remove(self, value):
        for i in range(len(self)):
            if self._item(i) == value:
                self.__delitem__(i)
                return
        raise ValueError("array.remove(x): x not in array")

    def reverse(self):
        items = list(self)
        items.reverse()
        self._buffer = bytearray()
        for item in items:
            self.append(item)

    def count(self, value):
        total = 0
        for item in self:
            if item == value:
                total += 1
        return total

    def index(self, value):
        for i in range(len(self)):
            if self._item(i) == value:
                return i
        raise ValueError("array.index(x): x not in array")

    # --- bytes in and out ---

    def tobytes(self):
        return bytes(self._buffer)

    def frombytes(self, data):
        if not isinstance(data, (bytes, bytearray)):
            raise TypeError("a bytes-like object is required, not '" + type(data).__name__ + "'")
        if len(data) % self.itemsize != 0:
            raise ValueError("bytes length not a multiple of item size")
        self._buffer = self._buffer + bytearray(data)

    def tolist(self):
        return list(self)

    def fromlist(self, values):
        if not isinstance(values, list):
            raise TypeError("arg must be list")
        for value in values:
            self.append(value)

    def buffer_info(self):
        # The address half is meaningless without a stable object address, so this reports the length
        # and refuses to invent the pointer rather than returning a plausible zero.
        raise NotImplementedError("buffer_info() needs a stable buffer address, which this runtime does not expose")


typecodes = _TYPECODES
