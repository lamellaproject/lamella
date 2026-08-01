"""JSON encoding and decoding, after RFC 8259 and CPython's json package.

CPython's json is pure Python with an optional C accelerator.

Two deliberate departures from CPython's implementation:

* No `re`. CPython's scanner matches strings and numbers with regular expressions;
  this hand-rolls that scanning. The grammar and the error messages/positions agree.
* Circular references are detected with an ancestor stack compared by identity,
  not a dict keyed by `id()`. A moving collector may relocate an object mid-encode,
  which would invalidate an id-keyed marker; the ancestor stack cannot go stale.
  CPython drops each marker once the container is encoded, so only a true ancestor
  cycle is a ValueError either way -- a repeated sibling reference encodes twice.

A LONE surrogate escape (`"\\ud834"` with no trailing low surrogate) decodes to a
one-character string in CPython but raises ValueError here: a str holds UTF-8, which
has no representation for an unpaired surrogate. A well-formed pair still decodes to
its code point, and every other escape agrees.
"""

__all__ = [
    "dump",
    "dumps",
    "load",
    "loads",
    "JSONDecoder",
    "JSONDecodeError",
    "JSONEncoder",
]

_INFINITY = float("inf")
_NAN = float("nan")

_ESCAPE_DCT = {
    "\\": "\\\\",
    '"': '\\"',
    "\b": "\\b",
    "\f": "\\f",
    "\n": "\\n",
    "\r": "\\r",
    "\t": "\\t",
}

_UNESCAPE_DCT = {
    '"': '"',
    "\\": "\\",
    "/": "/",
    "b": "\b",
    "f": "\f",
    "n": "\n",
    "r": "\r",
    "t": "\t",
}

_WHITESPACE = " \t\n\r"
_DIGITS = "0123456789"


class JSONDecodeError(ValueError):
    def __init__(self, msg, doc, pos):
        lineno = doc.count("\n", 0, pos) + 1
        colno = pos - doc.rfind("\n", 0, pos)
        errmsg = "%s: line %d column %d (char %d)" % (msg, lineno, colno, pos)
        super().__init__(errmsg)
        self.msg = msg
        self.doc = doc
        self.pos = pos
        self.lineno = lineno
        self.colno = colno


def _encode_basestring(s):
    """Return a JSON string literal, escaping only what JSON requires."""
    parts = ['"']
    for char in s:
        escape = _ESCAPE_DCT.get(char)
        if escape is not None:
            parts.append(escape)
        elif ord(char) < 0x20:
            parts.append("\\u%04x" % ord(char))
        else:
            parts.append(char)
    parts.append('"')
    return "".join(parts)


def _encode_basestring_ascii(s):
    """Return a JSON string literal with every non-ASCII character escaped."""
    parts = ['"']
    for char in s:
        escape = _ESCAPE_DCT.get(char)
        if escape is not None:
            parts.append(escape)
            continue
        point = ord(char)
        if 0x20 <= point <= 0x7E:
            parts.append(char)
        elif point < 0x10000:
            parts.append("\\u%04x" % point)
        else:
            # A code point outside the BMP is written as a UTF-16 surrogate pair.
            point -= 0x10000
            high = 0xD800 | ((point >> 10) & 0x3FF)
            low = 0xDC00 | (point & 0x3FF)
            parts.append("\\u%04x\\u%04x" % (high, low))
    parts.append('"')
    return "".join(parts)


class JSONEncoder:
    item_separator = ", "
    key_separator = ": "

    def __init__(
        self,
        *,
        skipkeys=False,
        ensure_ascii=True,
        check_circular=True,
        allow_nan=True,
        sort_keys=False,
        indent=None,
        separators=None,
        default=None,
    ):
        self.skipkeys = skipkeys
        self.ensure_ascii = ensure_ascii
        self.check_circular = check_circular
        self.allow_nan = allow_nan
        self.sort_keys = sort_keys
        self.indent = indent
        if separators is not None:
            self.item_separator, self.key_separator = separators
        elif indent is not None:
            self.item_separator = ","
        if default is not None:
            self.default = default

    def default(self, o):
        raise TypeError(
            "Object of type " + o.__class__.__name__ + " is not JSON serializable"
        )

    def encode(self, o):
        if isinstance(o, str):
            if self.ensure_ascii:
                return _encode_basestring_ascii(o)
            return _encode_basestring(o)
        return "".join(self.iterencode(o))

    def iterencode(self, o):
        indent = self.indent
        if indent is not None and not isinstance(indent, str):
            indent = " " * indent
        ancestors = [] if self.check_circular else None
        return self._iterencode(o, 0, indent, ancestors)

    def _encode_string(self, s):
        if self.ensure_ascii:
            return _encode_basestring_ascii(s)
        return _encode_basestring(s)

    def _floatstr(self, o):
        if o != o:
            text = "NaN"
        elif o == _INFINITY:
            text = "Infinity"
        elif o == -_INFINITY:
            text = "-Infinity"
        else:
            return repr(o)
        if not self.allow_nan:
            raise ValueError(
                "Out of range float values are not JSON compliant: " + repr(o)
            )
        return text

    def _enter(self, ancestors, o):
        if ancestors is None:
            return
        for seen in ancestors:
            if seen is o:
                raise ValueError("Circular reference detected")
        ancestors.append(o)

    def _leave(self, ancestors):
        if ancestors is not None:
            ancestors.pop()

    def _iterencode(self, o, level, indent, ancestors):
        if isinstance(o, str):
            yield self._encode_string(o)
        elif o is None:
            yield "null"
        elif o is True:
            yield "true"
        elif o is False:
            yield "false"
        elif isinstance(o, int):
            yield repr(o)
        elif isinstance(o, float):
            yield self._floatstr(o)
        elif isinstance(o, (list, tuple)):
            yield from self._iterencode_list(o, level, indent, ancestors)
        elif isinstance(o, dict):
            yield from self._iterencode_dict(o, level, indent, ancestors)
        else:
            self._enter(ancestors, o)
            yield from self._iterencode(self.default(o), level, indent, ancestors)
            self._leave(ancestors)

    def _iterencode_list(self, lst, level, indent, ancestors):
        if not lst:
            yield "[]"
            return
        self._enter(ancestors, lst)
        buf = "["
        if indent is not None:
            level += 1
            newline_indent = "\n" + indent * level
            separator = self.item_separator + newline_indent
            buf += newline_indent
        else:
            newline_indent = None
            separator = self.item_separator
        first = True
        for value in lst:
            if first:
                first = False
            else:
                buf = separator
            yield buf
            yield from self._iterencode(value, level, indent, ancestors)
        if newline_indent is not None:
            level -= 1
            yield "\n" + indent * level
        yield "]"
        self._leave(ancestors)

    def _iterencode_dict(self, dct, level, indent, ancestors):
        if not dct:
            yield "{}"
            return
        self._enter(ancestors, dct)
        yield "{"
        if indent is not None:
            level += 1
            newline_indent = "\n" + indent * level
            yield newline_indent
        else:
            newline_indent = None
        item_separator = self.item_separator
        if newline_indent is not None:
            item_separator = item_separator + newline_indent
        first = True
        items = dct.items()
        if self.sort_keys:
            items = sorted(items)
        for key, value in items:
            if isinstance(key, str):
                pass
            elif isinstance(key, float):
                key = self._floatstr(key)
            elif key is True:
                key = "true"
            elif key is False:
                key = "false"
            elif key is None:
                key = "null"
            elif isinstance(key, int):
                key = repr(key)
            elif self.skipkeys:
                continue
            else:
                raise TypeError(
                    "keys must be str, int, float, bool or None, not "
                    + key.__class__.__name__
                )
            if first:
                first = False
            else:
                yield item_separator
            yield self._encode_string(key)
            yield self.key_separator
            yield from self._iterencode(value, level, indent, ancestors)
        if newline_indent is not None:
            level -= 1
            yield "\n" + indent * level
        yield "}"
        self._leave(ancestors)


def _scanstring(s, end, strict=True):
    """Scan a string literal whose opening quote is at `end` - 1; return (value, end)."""
    chunks = []
    begin = end - 1
    while True:
        if end >= len(s):
            raise JSONDecodeError("Unterminated string starting at", s, begin)
        terminator = s[end]
        if terminator == '"':
            return "".join(chunks), end + 1
        if terminator == "\\":
            escape_pos = end
            end += 1
            if end >= len(s):
                raise JSONDecodeError("Unterminated string starting at", s, begin)
            esc = s[end]
            if esc != "u":
                char = _UNESCAPE_DCT.get(esc)
                if char is None:
                    raise JSONDecodeError("Invalid \\escape", s, escape_pos)
                chunks.append(char)
                end += 1
                continue
            uni = _decode_uXXXX(s, end)
            end += 5
            # A high surrogate followed by a low one is one non-BMP code point.
            if 0xD800 <= uni <= 0xDBFF and s[end : end + 2] == "\\u":
                uni2 = _decode_uXXXX(s, end + 1)
                if 0xDC00 <= uni2 <= 0xDFFF:
                    uni = 0x10000 + (((uni - 0xD800) << 10) | (uni2 - 0xDC00))
                    end += 6
            chunks.append(chr(uni))
            continue
        if terminator < " " and strict:
            raise JSONDecodeError("Invalid control character at", s, end)
        chunks.append(terminator)
        end += 1


def _decode_uXXXX(s, pos):
    """Decode the four hex digits of a \\uXXXX escape whose 'u' is at `pos`."""
    esc = s[pos + 1 : pos + 5]
    if len(esc) == 4 and esc[1] not in "xX":
        try:
            return int(esc, 16)
        except ValueError:
            pass
    raise JSONDecodeError("Invalid \\uXXXX escape", s, pos)


def _skip_whitespace(s, end):
    while end < len(s) and s[end] in _WHITESPACE:
        end += 1
    return end


def _match_number(s, start):
    """Match a JSON number at `start`; return (end, is_float) or None."""
    end = start
    if end < len(s) and s[end] == "-":
        end += 1
    int_start = end
    if end < len(s) and s[end] == "0":
        end += 1
    elif end < len(s) and s[end] in "123456789":
        while end < len(s) and s[end] in _DIGITS:
            end += 1
    else:
        return None
    if end == int_start:
        return None
    is_float = False
    if end < len(s) and s[end] == ".":
        frac = end + 1
        while frac < len(s) and s[frac] in _DIGITS:
            frac += 1
        if frac > end + 1:
            is_float = True
            end = frac
    if end < len(s) and s[end] in "eE":
        exp = end + 1
        if exp < len(s) and s[exp] in "+-":
            exp += 1
        digits = exp
        while digits < len(s) and s[digits] in _DIGITS:
            digits += 1
        if digits > exp:
            is_float = True
            end = digits
    return end, is_float


class JSONDecoder:
    def __init__(
        self,
        *,
        object_hook=None,
        parse_float=None,
        parse_int=None,
        parse_constant=None,
        strict=True,
        object_pairs_hook=None,
    ):
        self.object_hook = object_hook
        self.parse_float = parse_float or float
        self.parse_int = parse_int or int
        self.parse_constant = parse_constant or _default_constant
        self.strict = strict
        self.object_pairs_hook = object_pairs_hook

    def decode(self, s):
        # Leading whitespace is skipped here, not in raw_decode: raw_decode documents
        # that it starts at exactly `idx`, so it reports "Expecting value" at 0 for a
        # document that begins with a space.
        obj, end = self.raw_decode(s, _skip_whitespace(s, 0))
        end = _skip_whitespace(s, end)
        if end != len(s):
            raise JSONDecodeError("Extra data", s, end)
        return obj

    def raw_decode(self, s, idx=0):
        return self._scan_once(s, idx)

    def _scan_once(self, s, idx):
        if idx >= len(s):
            raise JSONDecodeError("Expecting value", s, idx)
        nextchar = s[idx]
        if nextchar == '"':
            return _scanstring(s, idx + 1, self.strict)
        if nextchar == "{":
            return self._parse_object(s, idx + 1)
        if nextchar == "[":
            return self._parse_array(s, idx + 1)
        if nextchar == "n" and s[idx : idx + 4] == "null":
            return None, idx + 4
        if nextchar == "t" and s[idx : idx + 4] == "true":
            return True, idx + 4
        if nextchar == "f" and s[idx : idx + 5] == "false":
            return False, idx + 5

        matched = _match_number(s, idx)
        if matched is not None:
            end, is_float = matched
            text = s[idx:end]
            if is_float:
                return self.parse_float(text), end
            return self.parse_int(text), end
        if nextchar == "N" and s[idx : idx + 3] == "NaN":
            return self.parse_constant("NaN"), idx + 3
        if nextchar == "I" and s[idx : idx + 8] == "Infinity":
            return self.parse_constant("Infinity"), idx + 8
        if nextchar == "-" and s[idx : idx + 9] == "-Infinity":
            return self.parse_constant("-Infinity"), idx + 9
        raise JSONDecodeError("Expecting value", s, idx)

    def _parse_object(self, s, end):
        pairs = []
        end = _skip_whitespace(s, end)
        if end < len(s) and s[end] == "}":
            if self.object_pairs_hook is not None:
                return self.object_pairs_hook(pairs), end + 1
            result = {}
            if self.object_hook is not None:
                result = self.object_hook(result)
            return result, end + 1
        while True:
            end = _skip_whitespace(s, end)
            if end >= len(s) or s[end] != '"':
                raise JSONDecodeError(
                    "Expecting property name enclosed in double quotes", s, end
                )
            key, end = _scanstring(s, end + 1, self.strict)
            end = _skip_whitespace(s, end)
            if end >= len(s) or s[end] != ":":
                raise JSONDecodeError("Expecting ':' delimiter", s, end)
            end = _skip_whitespace(s, end + 1)
            value, end = self._scan_once(s, end)
            pairs.append((key, value))
            end = _skip_whitespace(s, end)
            if end < len(s) and s[end] == "}":
                end += 1
                break
            if end >= len(s) or s[end] != ",":
                raise JSONDecodeError("Expecting ',' delimiter", s, end)
            comma = end
            end = _skip_whitespace(s, end + 1)
            if end < len(s) and s[end] == "}":
                raise JSONDecodeError(
                    "Illegal trailing comma before end of object", s, comma
                )
        if self.object_pairs_hook is not None:
            return self.object_pairs_hook(pairs), end
        result = {}
        for key, value in pairs:
            result[key] = value
        if self.object_hook is not None:
            result = self.object_hook(result)
        return result, end

    def _parse_array(self, s, end):
        values = []
        end = _skip_whitespace(s, end)
        if end < len(s) and s[end] == "]":
            return values, end + 1
        while True:
            end = _skip_whitespace(s, end)
            value, end = self._scan_once(s, end)
            values.append(value)
            end = _skip_whitespace(s, end)
            if end < len(s) and s[end] == "]":
                return values, end + 1
            if end >= len(s) or s[end] != ",":
                raise JSONDecodeError("Expecting ',' delimiter", s, end)
            comma = end
            end = _skip_whitespace(s, end + 1)
            if end < len(s) and s[end] == "]":
                raise JSONDecodeError(
                    "Illegal trailing comma before end of array", s, comma
                )


def _default_constant(name):
    if name == "NaN":
        return _NAN
    if name == "Infinity":
        return _INFINITY
    return -_INFINITY


def dumps(
    obj,
    *,
    skipkeys=False,
    ensure_ascii=True,
    check_circular=True,
    allow_nan=True,
    cls=None,
    indent=None,
    separators=None,
    default=None,
    sort_keys=False,
    **kw,
):
    """Return `obj` as a JSON-formatted str."""
    if cls is None:
        cls = JSONEncoder
    return cls(
        skipkeys=skipkeys,
        ensure_ascii=ensure_ascii,
        check_circular=check_circular,
        allow_nan=allow_nan,
        indent=indent,
        separators=separators,
        default=default,
        sort_keys=sort_keys,
        **kw,
    ).encode(obj)


def dump(
    obj,
    fp,
    *,
    skipkeys=False,
    ensure_ascii=True,
    check_circular=True,
    allow_nan=True,
    cls=None,
    indent=None,
    separators=None,
    default=None,
    sort_keys=False,
    **kw,
):
    """Serialize `obj` as JSON to `fp`, a file-like object with a `write` method."""
    if cls is None:
        cls = JSONEncoder
    iterable = cls(
        skipkeys=skipkeys,
        ensure_ascii=ensure_ascii,
        check_circular=check_circular,
        allow_nan=allow_nan,
        indent=indent,
        separators=separators,
        default=default,
        sort_keys=sort_keys,
        **kw,
    ).iterencode(obj)
    for chunk in iterable:
        fp.write(chunk)


def loads(
    s,
    *,
    cls=None,
    object_hook=None,
    parse_float=None,
    parse_int=None,
    parse_constant=None,
    object_pairs_hook=None,
    **kw,
):
    """Deserialize a JSON document held in a str, bytes or bytearray."""
    if isinstance(s, str):
        if s.startswith("﻿"):
            raise JSONDecodeError(
                "Unexpected UTF-8 BOM (decode using utf-8-sig)", s, 0
            )
    else:
        if not isinstance(s, (bytes, bytearray)):
            raise TypeError(
                "the JSON object must be str, bytes or bytearray, not "
                + s.__class__.__name__
            )
        s = s.decode("utf-8")
    if cls is None:
        cls = JSONDecoder
    return cls(
        object_hook=object_hook,
        parse_float=parse_float,
        parse_int=parse_int,
        parse_constant=parse_constant,
        object_pairs_hook=object_pairs_hook,
        **kw,
    ).decode(s)


def load(
    fp,
    *,
    cls=None,
    object_hook=None,
    parse_float=None,
    parse_int=None,
    parse_constant=None,
    object_pairs_hook=None,
    **kw,
):
    """Deserialize a JSON document read from `fp`, a file-like object."""
    return loads(
        fp.read(),
        cls=cls,
        object_hook=object_hook,
        parse_float=parse_float,
        parse_int=parse_int,
        parse_constant=parse_constant,
        object_pairs_hook=object_pairs_hook,
        **kw,
    )
