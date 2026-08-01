"""Regular expression matching -- a deliberate SUBSET of the standard `re`, with one rule.

Everything this module accepts behaves exactly as the standard module does. Everything it does
not implement is REFUSED, loudly, by name -- `re.error("lookahead is not supported")` -- and never
silently treated as ordinary text. That rule is what makes the subset safe to grow: a pattern that
is refused today and supported later cannot change the meaning of a program that works, whereas a
construct quietly matched the wrong way can.

Supported: literals, `.`, `^`, `$`, `\\A`, `\\Z`, `\\b`, `\\B`, the classes `\\d \\D \\w \\W \\s \\S`,
character sets with ranges and negation, groups `(...)` and `(?:...)`, alternation, the repeats
`* + ? {m} {m,} {m,n}` and their non-greedy forms, and the escapes `\\n \\t \\r \\f \\v \\0 \\xHH`.
Verbs: `compile`, `match`, `fullmatch`, `search`, `findall`, `finditer`, `sub`, `subn`, `split`,
`escape`, and the `Match` accessors `group`, `groups`, `start`, `end`, `span`.

Refused with a named error: backreferences, named groups, lookahead and lookbehind, inline flags,
conditionals, possessive quantifiers, and every flag argument (`IGNORECASE` and friends).

Character classes follow the standard module's own definitions rather than an ASCII approximation:
`\\d` is a decimal digit, `\\w` is alphanumeric or underscore, `\\s` is whitespace -- each decided by
the string predicates, so they classify the way the rest of the language does.
"""


class error(Exception):
    pass


# Flag values, defined so passing one produces an explanation rather than an AttributeError.
NOFLAG = 0
IGNORECASE = 2
I = 2
LOCALE = 4
L = 4
MULTILINE = 8
M = 8
DOTALL = 16
S = 16
UNICODE = 32
U = 32
VERBOSE = 64
X = 64

_FLAG_NAMES = [
    [2, "IGNORECASE"],
    [4, "LOCALE"],
    [8, "MULTILINE"],
    [16, "DOTALL"],
    [32, "UNICODE"],
    [64, "VERBOSE"],
]

# Instructions. A program is a list of lists; the first item is the opcode.
_CHAR = 0
_ANY = 1
_SET = 2
_SPLIT = 3
_JMP = 4
_SAVE = 5
_MATCH = 6
_BOL = 7
_EOL = 8
_BEGIN = 9
_END = 10
_WORDB = 11
_NWORDB = 12
_MARK = 13
_GUARD = 14

_SPECIAL = "()[]{}?*+-|^$\\.&~# \t\n\r\v\f"


def _is_word_char(c):
    return c.isalnum() or c == "_"


def _class_matches(kind, c):
    if kind == "d":
        return c.isdecimal()
    if kind == "D":
        return not c.isdecimal()
    if kind == "w":
        return _is_word_char(c)
    if kind == "W":
        return not _is_word_char(c)
    if kind == "s":
        return c.isspace()
    return not c.isspace()


def _set_matches(items, negated, c):
    found = False
    for item in items:
        if item[0] == "r":
            if item[1] <= c and c <= item[2]:
                found = True
                break
        else:
            if _class_matches(item[1], c):
                found = True
                break
    if negated:
        return not found
    return found


class _Parser:
    def __init__(self, pattern):
        self.pattern = pattern
        self.pos = 0
        self.ngroups = 0

    def eof(self):
        return self.pos >= len(self.pattern)

    def peek(self):
        return self.pattern[self.pos]

    def take(self):
        c = self.pattern[self.pos]
        self.pos = self.pos + 1
        return c

    def fail(self, message):
        raise error(message + " at position " + str(self.pos))

    def parse(self):
        node = self.parse_alt()
        if not self.eof():
            self.fail("unbalanced parenthesis")
        return node

    def parse_alt(self):
        branches = [self.parse_cat()]
        while not self.eof() and self.peek() == "|":
            self.take()
            branches.append(self.parse_cat())
        if len(branches) == 1:
            return branches[0]
        return ["alt", branches]

    def parse_cat(self):
        items = []
        while not self.eof():
            c = self.peek()
            if c == "|" or c == ")":
                break
            items.append(self.parse_repeat())
        if len(items) == 1:
            return items[0]
        return ["cat", items]

    def parse_repeat(self):
        atom = self.parse_atom()
        while not self.eof():
            c = self.peek()
            low = 0
            high = -1
            if c == "*":
                self.take()
            elif c == "+":
                self.take()
                low = 1
            elif c == "?":
                self.take()
                high = 1
            elif c == "{":
                bounds = self.try_braces()
                if bounds is None:
                    break
                low = bounds[0]
                high = bounds[1]
            else:
                break
            if atom[0] == "rep":
                self.fail("multiple repeat")
            if atom[0] == "bol" or atom[0] == "eol":
                self.fail("nothing to repeat")
            greedy = True
            if not self.eof() and self.peek() == "?":
                self.take()
                greedy = False
            elif not self.eof() and self.peek() == "+":
                self.fail("possessive quantifiers are not supported")
            atom = ["rep", atom, low, high, greedy]
        return atom

    def try_braces(self):
        start = self.pos
        self.take()  # '{'
        digits = ""
        while not self.eof() and self.peek().isdecimal():
            digits = digits + self.take()
        if self.eof():
            self.pos = start
            return None
        if digits == "" and self.peek() != ",":
            self.pos = start
            return None
        c = self.take()
        if c == "}":
            if digits == "":
                self.pos = start
                return None
            n = int(digits)
            return [n, n]
        if c != ",":
            self.pos = start
            return None
        upper = ""
        while not self.eof() and self.peek().isdecimal():
            upper = upper + self.take()
        if self.eof() or self.take() != "}":
            self.pos = start
            return None
        low = 0
        if digits != "":
            low = int(digits)
        high = -1
        if upper != "":
            high = int(upper)
        if high != -1 and high < low:
            raise error("min repeat greater than max repeat")
        return [low, high]

    def parse_atom(self):
        c = self.take()
        if c == "(":
            return self.parse_group()
        if c == "[":
            return self.parse_set()
        if c == ".":
            return ["any"]
        if c == "^":
            return ["bol"]
        if c == "$":
            return ["eol"]
        if c == "\\":
            return self.parse_escape()
        if c == "*" or c == "+" or c == "?":
            raise error("nothing to repeat at position " + str(self.pos - 1))
        return ["char", c]

    def parse_group(self):
        if not self.eof() and self.peek() == "?":
            self.take()
            if self.eof():
                self.fail("unexpected end of pattern")
            kind = self.take()
            if kind == ":":
                node = self.parse_alt()
                self.expect_close()
                return node
            if kind == "P":
                raise error("named groups are not supported")
            if kind == "=" or kind == "!":
                raise error("lookahead is not supported")
            if kind == "<":
                raise error("lookbehind is not supported")
            if kind == "#":
                raise error("pattern comments are not supported")
            if kind == "(":
                raise error("conditional matching is not supported")
            raise error("inline flags are not supported")
        self.ngroups = self.ngroups + 1
        index = self.ngroups
        node = self.parse_alt()
        self.expect_close()
        return ["group", index, node]

    def expect_close(self):
        if self.eof() or self.take() != ")":
            raise error("missing ), unterminated subpattern")

    def parse_set(self):
        negated = False
        if not self.eof() and self.peek() == "^":
            self.take()
            negated = True
        items = []
        first = True
        while True:
            if self.eof():
                raise error("unterminated character set")
            c = self.take()
            if c == "]" and not first:
                break
            first = False
            if c == "\\":
                item = self.parse_set_escape()
                if item[0] == "c":
                    c = item[1]
                else:
                    items.append(item)
                    continue
            if not self.eof() and self.peek() == "-":
                if self.pos + 1 < len(self.pattern) and self.pattern[self.pos + 1] != "]":
                    self.take()
                    high = self.take()
                    if high == "\\":
                        esc = self.parse_set_escape()
                        if esc[0] != "c":
                            raise error("bad character range")
                        high = esc[1]
                    if high < c:
                        raise error("bad character range " + c + "-" + high)
                    items.append(["r", c, high])
                    continue
            items.append(["r", c, c])
        if len(items) == 0:
            raise error("empty character set")
        return ["set", items, negated]

    def parse_set_escape(self):
        if self.eof():
            raise error("bad escape (end of pattern)")
        c = self.take()
        if c == "d" or c == "D" or c == "w" or c == "W" or c == "s" or c == "S":
            return ["p", c]
        return ["c", self.literal_escape(c)]

    def parse_escape(self):
        if self.eof():
            raise error("bad escape (end of pattern)")
        c = self.take()
        if c == "d" or c == "D" or c == "w" or c == "W" or c == "s" or c == "S":
            return ["set", [["p", c]], False]
        if c == "b":
            return ["wordb"]
        if c == "B":
            return ["nwordb"]
        if c == "A":
            return ["begin"]
        if c == "Z":
            return ["end"]
        if c.isdecimal():
            raise error("backreferences are not supported")
        return ["char", self.literal_escape(c)]

    def literal_escape(self, c):
        if c == "n":
            return "\n"
        if c == "t":
            return "\t"
        if c == "r":
            return "\r"
        if c == "f":
            return "\f"
        if c == "v":
            return "\v"
        if c == "a":
            return "\a"
        if c == "0":
            return "\0"
        if c == "x":
            digits = ""
            while len(digits) < 2 and not self.eof():
                digits = digits + self.take()
            if len(digits) != 2:
                raise error("incomplete escape \\x")
            return chr(int(digits, 16))
        if c.isalnum():
            raise error("bad escape \\" + c)
        return c


def _nullable(node):
    tag = node[0]
    if tag == "cat":
        for sub in node[1]:
            if not _nullable(sub):
                return False
        return True
    if tag == "alt":
        for sub in node[1]:
            if _nullable(sub):
                return True
        return False
    if tag == "group":
        return _nullable(node[2])
    if tag == "rep":
        if node[2] == 0:
            return True
        return _nullable(node[1])
    if tag == "char" or tag == "any" or tag == "set":
        return False
    return True  # anchors and boundaries consume nothing


class _Emitter:
    def __init__(self):
        self.prog = []
        self.marks = 0

    def emit(self, node):
        tag = node[0]
        if tag == "char":
            self.prog.append([_CHAR, node[1]])
        elif tag == "any":
            self.prog.append([_ANY])
        elif tag == "set":
            self.prog.append([_SET, node[1], node[2]])
        elif tag == "bol":
            self.prog.append([_BOL])
        elif tag == "eol":
            self.prog.append([_EOL])
        elif tag == "begin":
            self.prog.append([_BEGIN])
        elif tag == "end":
            self.prog.append([_END])
        elif tag == "wordb":
            self.prog.append([_WORDB])
        elif tag == "nwordb":
            self.prog.append([_NWORDB])
        elif tag == "cat":
            for sub in node[1]:
                self.emit(sub)
        elif tag == "group":
            self.prog.append([_SAVE, node[1] * 2])
            self.emit(node[2])
            self.prog.append([_SAVE, node[1] * 2 + 1])
        elif tag == "alt":
            self.emit_alt(node[1])
        else:
            self.emit_rep(node)

    def emit_alt(self, branches):
        jumps = []
        index = 0
        while index < len(branches) - 1:
            split = len(self.prog)
            self.prog.append([_SPLIT, 0, 0])
            self.prog[split][1] = len(self.prog)
            self.emit(branches[index])
            jumps.append(len(self.prog))
            self.prog.append([_JMP, 0])
            self.prog[split][2] = len(self.prog)
            index = index + 1
        self.emit(branches[len(branches) - 1])
        for site in jumps:
            self.prog[site][1] = len(self.prog)

    def emit_rep(self, node):
        body = node[1]
        low = node[2]
        high = node[3]
        greedy = node[4]
        count = 0
        while count < low:
            self.emit(body)
            count = count + 1
        if high == -1:
            self.emit_star(body, greedy)
        else:
            count = low
            while count < high:
                self.emit_optional(body, greedy)
                count = count + 1

    def emit_optional(self, body, greedy):
        split = len(self.prog)
        self.prog.append([_SPLIT, 0, 0])
        body_at = len(self.prog)
        self.emit(body)
        after = len(self.prog)
        if greedy:
            self.prog[split][1] = body_at
            self.prog[split][2] = after
        else:
            self.prog[split][1] = after
            self.prog[split][2] = body_at

    def emit_star(self, body, greedy):
        # A body that can match nothing would loop forever, so such a loop carries a mark and
        # LEAVES once an iteration consumes nothing -- a pattern like `(a*)*` is easy to write by
        # accident. It leaves rather than failing that iteration, because the standard module runs
        # the empty iteration and keeps what it captured: `(a*)*` against "aaa" reports group 1 as
        # empty, not "aaa". Failing the iteration instead would report the previous one's text.
        guard = _nullable(body)
        split = len(self.prog)
        self.prog.append([_SPLIT, 0, 0])
        body_at = len(self.prog)
        mark = -1
        close = -1
        if guard:
            mark = self.marks
            self.marks = self.marks + 1
            self.prog.append([_MARK, mark])
        self.emit(body)
        if guard:
            close = len(self.prog)
            self.prog.append([_GUARD, mark, 0, split])
        else:
            self.prog.append([_JMP, split])
        after = len(self.prog)
        if guard:
            self.prog[close][2] = after
        if greedy:
            self.prog[split][1] = body_at
            self.prog[split][2] = after
        else:
            self.prog[split][1] = after
            self.prog[split][2] = body_at


def _compile_program(pattern):
    parser = _Parser(pattern)
    node = parser.parse()
    emitter = _Emitter()
    emitter.prog.append([_SAVE, 0])
    emitter.emit(node)
    emitter.prog.append([_SAVE, 1])
    emitter.prog.append([_MATCH])
    return [emitter.prog, parser.ngroups, emitter.marks]


def _run(prog, slot_count, mark_base, string, start, must_end):
    slots = []
    index = 0
    while index < slot_count:
        slots.append(-1)
        index = index + 1
    stack = []
    pc = 0
    sp = start
    size = len(string)
    while True:
        step = prog[pc]
        op = step[0]
        alive = True
        if op == _CHAR:
            if sp < size and string[sp] == step[1]:
                sp = sp + 1
                pc = pc + 1
            else:
                alive = False
        elif op == _ANY:
            if sp < size and string[sp] != "\n":
                sp = sp + 1
                pc = pc + 1
            else:
                alive = False
        elif op == _SET:
            if sp < size and _set_matches(step[1], step[2], string[sp]):
                sp = sp + 1
                pc = pc + 1
            else:
                alive = False
        elif op == _SPLIT:
            stack.append([step[2], sp, slots[:]])
            pc = step[1]
        elif op == _JMP:
            pc = step[1]
        elif op == _SAVE:
            slots[step[1]] = sp
            pc = pc + 1
        elif op == _MARK:
            slots[mark_base + step[1]] = sp
            pc = pc + 1
        elif op == _GUARD:
            if slots[mark_base + step[1]] == sp:
                pc = step[2]
            else:
                pc = step[3]
        elif op == _BOL or op == _BEGIN:
            if sp == 0:
                pc = pc + 1
            else:
                alive = False
        elif op == _EOL:
            if sp == size or (sp == size - 1 and string[sp] == "\n"):
                pc = pc + 1
            else:
                alive = False
        elif op == _END:
            if sp == size:
                pc = pc + 1
            else:
                alive = False
        elif op == _WORDB or op == _NWORDB:
            before = sp > 0 and _is_word_char(string[sp - 1])
            after = sp < size and _is_word_char(string[sp])
            at_boundary = before != after
            if op == _NWORDB:
                at_boundary = not at_boundary
            if at_boundary:
                pc = pc + 1
            else:
                alive = False
        else:
            if must_end and sp != size:
                alive = False
            else:
                return slots
        if not alive:
            if len(stack) == 0:
                return None
            frame = stack.pop()
            pc = frame[0]
            sp = frame[1]
            slots = frame[2]


class Match:
    def __init__(self, string, slots, pattern, pos, endpos):
        self.string = string
        self._slots = slots
        self.re = pattern
        self.pos = pos
        self.endpos = endpos
        self.lastindex = None
        index = 1
        while index <= pattern.groups:
            if slots[index * 2] != -1 and slots[index * 2 + 1] != -1:
                self.lastindex = index
            index = index + 1

    def _check(self, index):
        if index < 0 or index > self.re.groups:
            raise IndexError("no such group")
        return index

    def __copy__(self):
        return self

    def __deepcopy__(self, memo):
        return self

    def group(self, *args):
        if len(args) == 0:
            return self._one(0)
        if len(args) == 1:
            return self._one(args[0])
        out = []
        for index in args:
            out.append(self._one(index))
        return tuple(out)

    def _one(self, index):
        self._check(index)
        begin = self._slots[index * 2]
        finish = self._slots[index * 2 + 1]
        if begin == -1 or finish == -1:
            return None
        return self.string[begin:finish]

    def groups(self, default=None):
        out = []
        index = 1
        while index <= self.re.groups:
            value = self._one(index)
            if value is None:
                value = default
            out.append(value)
            index = index + 1
        return tuple(out)

    def start(self, group=0):
        self._check(group)
        return self._slots[group * 2]

    def end(self, group=0):
        self._check(group)
        return self._slots[group * 2 + 1]

    def span(self, group=0):
        return (self.start(group), self.end(group))

    def __repr__(self):
        return "<re.Match object; span=" + repr(self.span()) + ", match=" + repr(self.group()) + ">"


class Pattern:
    def __init__(self, pattern, prog, ngroups, marks):
        self.pattern = pattern
        self.groups = ngroups
        self._prog = prog
        self._mark_base = (ngroups + 1) * 2
        self._slot_count = self._mark_base + marks

    def _match_at(self, string, start, must_end):
        slots = _run(self._prog, self._slot_count, self._mark_base, string, start, must_end)
        if slots is None:
            return None
        return Match(string, slots, self, 0, len(string))

    def __copy__(self):
        return self

    def __deepcopy__(self, memo):
        return self

    def match(self, string, pos=0):
        return self._match_at(string, pos, False)

    def fullmatch(self, string, pos=0):
        return self._match_at(string, pos, True)

    def search(self, string, pos=0):
        start = pos
        while start <= len(string):
            found = self._match_at(string, start, False)
            if found is not None:
                return found
            start = start + 1
        return None

    def finditer(self, string):
        position = 0
        while position <= len(string):
            found = self.search(string, position)
            if found is None:
                return
            yield found
            if found.end() == found.start():
                position = found.end() + 1
            else:
                position = found.end()

    def findall(self, string):
        # A group that did not take part reads as "" here, though `groups()` reports it as None and
        # `split` keeps the None -- that difference belongs to this verb, not to the match.
        out = []
        for found in self.finditer(string):
            if self.groups == 0:
                out.append(found.group())
            elif self.groups == 1:
                value = found.group(1)
                if value is None:
                    value = ""
                out.append(value)
            else:
                out.append(found.groups(""))
        return out

    def subn(self, repl, string, count=0):
        pieces = []
        last = 0
        made = 0
        for found in self.finditer(string):
            if count != 0 and made >= count:
                break
            pieces.append(string[last:found.start()])
            pieces.append(_expand(repl, found))
            last = found.end()
            made = made + 1
        pieces.append(string[last:])
        return ("".join(pieces), made)

    def sub(self, repl, string, count=0):
        return self.subn(repl, string, count)[0]

    def split(self, string, maxsplit=0):
        pieces = []
        last = 0
        made = 0
        for found in self.finditer(string):
            if maxsplit != 0 and made >= maxsplit:
                break
            pieces.append(string[last:found.start()])
            index = 1
            while index <= self.groups:
                pieces.append(found.group(index))
                index = index + 1
            last = found.end()
            made = made + 1
        pieces.append(string[last:])
        return pieces

    def __repr__(self):
        return "re.compile(" + repr(self.pattern) + ")"


def _expand(repl, found):
    if not isinstance(repl, str):
        return repl(found)
    out = []
    index = 0
    while index < len(repl):
        c = repl[index]
        if c != "\\":
            out.append(c)
            index = index + 1
            continue
        index = index + 1
        if index >= len(repl):
            raise error("bad escape (end of pattern)")
        c = repl[index]
        index = index + 1
        if c.isdecimal():
            digits = c
            while index < len(repl) and repl[index].isdecimal() and len(digits) < 2:
                digits = digits + repl[index]
                index = index + 1
            value = found.group(int(digits))
            if value is not None:
                out.append(value)
        elif c == "g":
            if index >= len(repl) or repl[index] != "<":
                raise error("missing <")
            index = index + 1
            name = ""
            while index < len(repl) and repl[index] != ">":
                name = name + repl[index]
                index = index + 1
            if index >= len(repl):
                raise error("missing >, unterminated name")
            index = index + 1
            if not name.isdecimal():
                raise error("named groups are not supported")
            value = found.group(int(name))
            if value is not None:
                out.append(value)
        elif c == "n":
            out.append("\n")
        elif c == "t":
            out.append("\t")
        elif c == "r":
            out.append("\r")
        elif c == "\\":
            out.append("\\")
        else:
            raise error("bad escape \\" + c)
    return "".join(out)


_cache = {}


def _reject_flags(flags):
    if flags == 0:
        return
    for entry in _FLAG_NAMES:
        if flags & entry[0]:
            raise error(entry[1] + " is not supported")
    raise error("flags are not supported")


def compile(pattern, flags=0):
    _reject_flags(flags)
    if isinstance(pattern, Pattern):
        return pattern
    if pattern in _cache:
        return _cache[pattern]
    built = _compile_program(pattern)
    compiled = Pattern(pattern, built[0], built[1], built[2])
    _cache[pattern] = compiled
    return compiled


def match(pattern, string, flags=0):
    return compile(pattern, flags).match(string)


def fullmatch(pattern, string, flags=0):
    return compile(pattern, flags).fullmatch(string)


def search(pattern, string, flags=0):
    return compile(pattern, flags).search(string)


def findall(pattern, string, flags=0):
    return compile(pattern, flags).findall(string)


def finditer(pattern, string, flags=0):
    return compile(pattern, flags).finditer(string)


def sub(pattern, repl, string, count=0, flags=0):
    return compile(pattern, flags).sub(repl, string, count)


def subn(pattern, repl, string, count=0, flags=0):
    return compile(pattern, flags).subn(repl, string, count)


def split(pattern, string, maxsplit=0, flags=0):
    return compile(pattern, flags).split(string, maxsplit)


def purge():
    global _cache
    _cache = {}


def escape(pattern):
    out = []
    for c in pattern:
        if c in _SPECIAL:
            out.append("\\")
        out.append(c)
    return "".join(out)
