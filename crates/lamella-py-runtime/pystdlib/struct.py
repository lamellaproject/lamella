# struct, bundled as a MANAGED module. All five modes: '<' little-endian, '>' big-endian, '!' network
# (= big-endian), '=' native byte order with standard sizes, and '@' NATIVE -- native sizes AND the
# alignment padding the platform's C compiler would insert. A format with no byte-order character is
# '@', as it is in CPython. Integer (b B h H i I l L q Q, plus native-only n N P), bool (?), char (c),
# string (s) and pad (x) codes are byte-identical to CPython. Signedness is done by two's-complement
# masking, so int.to_bytes(signed=) is not needed.
#
# '@' is deliberately platform-DEPENDENT: `calcsize("@ci")` is 8 where an int aligns to 4, and `long`
# is 4 bytes under LLP64 and on a 32-bit device but 8 under LP64. Those facts are read at run time from
# the `_platform` seam, which derives them from the compiler that built the runtime -- so the answer is
# the one the machine actually running the program would give, never a remembered one.
#
# The float codes f (single) and d (double) go through a tiny native seam, because seeing a float's
# actual bits is the one thing pure Python cannot do. Packing a double as 'f' rounds to single
# precision exactly as a C cast would, including overflow to infinity, which is not an error.
#
# NOT PROVIDED (each raises a clear error rather than diverging):
#   - the half-precision code 'e', which needs its own 16-bit conversion, and 'p' (the Pascal string).
#   - pack_into / unpack_from, which write through a caller's buffer.
import _platform
import _struct


class error(Exception):
    pass


# The C type each code is, for native mode; the seam supplies its size and alignment.
_C_TYPE = {
    "b": "char",
    "B": "char",
    "c": "char",
    "?": "bool",
    "h": "short",
    "H": "short",
    "i": "int",
    "I": "int",
    "l": "long",
    "L": "long",
    "q": "long long",
    "Q": "long long",
    "n": "size_t",
    "N": "size_t",
    "P": "void*",
    "f": "float",
    "d": "double",
}

# The float codes and their standard-mode widths.
_FLOAT = {"f": 4, "d": 8}

# Codes that exist ONLY in native mode -- CPython rejects them under an explicit byte order, because
# their width is a platform fact and a standard-mode format is meant to be portable.
_NATIVE_ONLY = "nNP"


# code -> (size in bytes, is_signed) for the standard integer codes. '?', 's', 'x' are special-cased.
_INT = {
    "b": (1, True),
    "B": (1, False),
    "h": (2, True),
    "H": (2, False),
    "i": (4, True),
    "I": (4, False),
    "l": (4, True),
    "L": (4, False),
    "q": (8, True),
    "Q": (8, False),
}

_DIGITS = "0123456789"
_SPACE = " \t\n"


def _byteorder(fmt):
    # Returns (order, body, native): order is 'little' or 'big', body is the format past any
    # byte-order character, and native says whether sizes and alignment come from the platform.
    # A format with no byte-order character is native, as it is in CPython.
    if not fmt:
        return _platform.byteorder, "", True
    if fmt[0] in "<=":
        order = _platform.byteorder if fmt[0] == "=" else "little"
        return order, fmt[1:], False
    if fmt[0] in ">!":
        return "big", fmt[1:], False
    if fmt[0] == "@":
        return _platform.byteorder, fmt[1:], True
    return _platform.byteorder, fmt, True


def _pad_to(offset, align):
    # The padding a native record needs before an item of this alignment. Standard modes never pad,
    # which is what makes them portable.
    if align <= 1:
        return 0
    over = offset % align
    if over == 0:
        return 0
    return align - over


def _items(body, native):
    # Parse the format body into a list of (code, count). A leading decimal is the count (the string
    # length for 's', the pad width for 'x', a repetition for the rest). An unknown code is rejected here
    # so a bad format is caught before any packing, exactly as CPython reports it.
    result = []
    i = 0
    n = len(body)
    while i < n:
        ch = body[i]
        if ch in _SPACE:
            i += 1
            continue
        count = 1
        if ch in _DIGITS:
            start = i
            while i < n and body[i] in _DIGITS:
                i += 1
            count = int(body[start:i])
            if i >= n:
                raise error("repeat count given without format specifier")
            ch = body[i]
        i += 1
        if ch == "e":
            raise error("format code 'e' (half precision) is not supported yet")
        if ch == "p":
            raise error("format code 'p' is not supported yet")
        if ch in _NATIVE_ONLY and not native:
            raise error("bad char in struct format")
        if ch not in _INT and ch not in _FLOAT and ch not in "sxc?" and ch not in _NATIVE_ONLY:
            raise error("bad char in struct format")
        # Per-element size and alignment. Standard modes use the portable sizes and never pad;
        # native mode asks the platform, so a record is laid out the way its C compiler would.
        align = 1
        if ch == "s" or ch == "x":
            size = 1
        elif native:
            c_type = _C_TYPE[ch]
            size = _platform.sizes[c_type]
            align = _platform.aligns[c_type]
        elif ch == "?" or ch == "c":
            size = 1
        elif ch in _FLOAT:
            size = _FLOAT[ch]
        else:
            size = _INT[ch][0]
        result.append((ch, count, size, align))
    return result


def _size(items):
    total = 0
    for code, count, size, align in items:
        total += _pad_to(total, align)
        total += size * count
    return total


def _needed(items):
    # The number of values pack() consumes: one per int/bool/char item, one per 's' group, none for 'x'.
    total = 0
    for code, count, size, align in items:
        if code == "x":
            continue
        total += 1 if code == "s" else count
    return total


def _signed(code):
    if code in _INT:
        return _INT[code][1]
    return code == "n"


def calcsize(fmt):
    order, body, native = _byteorder(fmt)
    return _size(_items(body, native))


def pack(fmt, *values):
    order, body, native = _byteorder(fmt)
    items = _items(body, native)
    needed = _needed(items)
    if len(values) != needed:
        raise error("pack expected %d items for packing (got %d)" % (needed, len(values)))
    out = b""
    vi = 0
    for code, count, size, align in items:
        out += b"\x00" * _pad_to(len(out), align)
        if code == "x":
            out += b"\x00" * count
        elif code == "c":
            for _ in range(count):
                v = values[vi]
                vi += 1
                if not isinstance(v, (bytes, bytearray)) or len(v) != 1:
                    raise error("char format requires a bytes object of length 1")
                out += bytes(v)
        elif code == "s":
            v = values[vi]
            vi += 1
            if not isinstance(v, (bytes, bytearray)):
                raise error("argument for 's' must be a bytes object")
            v = bytes(v[:count])
            out += v + b"\x00" * (count - len(v))
        elif code == "?":
            for _ in range(count):
                out += b"\x01" if values[vi] else b"\x00"
                vi += 1
        elif code in _FLOAT:
            for _ in range(count):
                v = values[vi]
                vi += 1
                if isinstance(v, bool) or not isinstance(v, (int, float)):
                    raise error("required argument is not a float")
                out += _struct.pack_float(float(v), size, order == "big")
        else:
            signed = _signed(code)
            lo = -(1 << (8 * size - 1)) if signed else 0
            hi = (1 << (8 * size - 1)) - 1 if signed else (1 << (8 * size)) - 1
            mask = (1 << (8 * size)) - 1
            for _ in range(count):
                v = values[vi]
                vi += 1
                if not isinstance(v, int):
                    raise error("required argument is not an integer")
                if v < lo or v > hi:
                    raise error("'%s' format requires %d <= number <= %d" % (code, lo, hi))
                out += (v & mask).to_bytes(size, order)
    return out


def unpack(fmt, buffer):
    order, body, native = _byteorder(fmt)
    items = _items(body, native)
    need = _size(items)
    if len(buffer) != need:
        raise error("unpack requires a buffer of %d bytes" % need)
    result = []
    off = 0
    for code, count, size, align in items:
        off += _pad_to(off, align)
        if code == "x":
            off += count
        elif code == "c":
            for _ in range(count):
                result.append(bytes(buffer[off:off + 1]))
                off += 1
        elif code == "s":
            result.append(bytes(buffer[off:off + count]))
            off += count
        elif code == "?":
            for _ in range(count):
                result.append(buffer[off] != 0)
                off += 1
        elif code in _FLOAT:
            for _ in range(count):
                result.append(_struct.unpack_float(bytes(buffer[off:off + size]), order == "big"))
                off += size
        else:
            signed = _signed(code)
            half = 1 << (8 * size - 1)
            whole = 1 << (8 * size)
            for _ in range(count):
                v = int.from_bytes(buffer[off:off + size], order)
                off += size
                if signed and v >= half:
                    v -= whole
                result.append(v)
    return tuple(result)


def iter_unpack(fmt, buffer):
    size = calcsize(fmt)
    if size == 0:
        raise error("cannot iter_unpack from a struct of size 0")
    if len(buffer) % size != 0:
        raise error("iterator requires a buffer of a multiple of %d bytes" % size)
    result = []
    for i in range(0, len(buffer), size):
        result.append(unpack(fmt, buffer[i:i + size]))
    return result


class Struct:
    def __init__(self, fmt):
        self.format = fmt
        self.size = calcsize(fmt)

    def pack(self, *values):
        return pack(self.format, *values)

    def unpack(self, buffer):
        return unpack(self.format, buffer)

    def iter_unpack(self, buffer):
        return iter_unpack(self.format, buffer)
