# The string module, bundled as a MANAGED module -- the ASCII character-class constants and capwords.
# Template and Formatter are NOT bundled: they need `re` / the `_string` C helper, which we do not
# have. The constants are plain literals (verified against CPython), and the differential verifies
# this module against CPython's real string module.

ascii_lowercase = "abcdefghijklmnopqrstuvwxyz"
ascii_uppercase = "ABCDEFGHIJKLMNOPQRSTUVWXYZ"
ascii_letters = ascii_lowercase + ascii_uppercase
digits = "0123456789"
hexdigits = digits + "abcdef" + "ABCDEF"
octdigits = "01234567"
punctuation = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~"
whitespace = " \t\n\r\x0b\x0c"
printable = digits + ascii_letters + punctuation + whitespace


def capwords(s, sep=None):
    """Return s with each word capitalized, as str.capitalize does it.

    The words are what s.split(sep) yields.  A missing or None sep splits on
    runs of whitespace and joins with one space; any other sep is used as-is
    both to split and to join.
    """
    return (sep or ' ').join(map(str.capitalize, s.split(sep)))
