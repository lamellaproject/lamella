"""os, bundled as a managed module over the native filesystem seam.

Only what needs a host underneath it is native (`_fs`: listing a directory, deleting, creating,
renaming, and asking what a path is). Everything here -- the path arithmetic, the predicates, the
walk -- is string handling, which needs nothing native and is clearer written once in Python.

A runtime whose embedder installed no filesystem raises from every verb rather than reporting an
empty directory: "there is nothing here" and "I cannot see" are different answers, and only one of
them is true.

`sep` is `/` and `linesep` is `\\n`, on every target. These are DELIBERATELY not the host's: this
runtime's targets are devices, `/` is accepted by every desktop platform as well, and a program that
writes `os.linesep` into a file should get the same bytes wherever it runs.

NOT PROVIDED, each raising by name rather than approximated: `stat` (a stat_result needs a whole
record of fields the seam does not carry), `getcwd`/`chdir` (a working directory is process state
this runtime does not own), `walk`, `environ`, and the process and permission surfaces.
"""
import _fs

#: The path separator. `/` on every target -- see the module docstring.
sep = "/"
#: The alternative separator; there is none.
altsep = None
#: The extension separator.
extsep = "."
#: The character that separates entries in a path list.
pathsep = ":"
#: The line ending `linesep` reports. `\\n` on every target -- see the module docstring.
linesep = "\n"
#: The null device's name.
devnull = "/dev/null"
#: The name of this operating-system flavour. Not "posix" or "nt": claiming either would invite a
#: program to assume the whole surface of one.
name = "lamella"

#: `os.path`, as an object with the same members the module has. `import os.path` is not supported;
#: `os.path.join(...)` is.
path = None


def listdir(target="."):
    """The names directly inside directory `target`, in host order (NOT sorted, as CPython)."""
    return _fs.listdir(target)


def remove(target):
    """Deletes the file `target`."""
    return _fs.remove(target)


unlink = remove


def mkdir(target, mode=0o777):
    """Creates the directory `target`. Its parent must exist; `mode` is accepted and unused."""
    return _fs.mkdir(target)


def makedirs(target, mode=0o777, exist_ok=False):
    """Creates `target` and every missing parent of it."""
    parent = path.dirname(target)
    if parent and parent != target and not path.exists(parent):
        makedirs(parent, mode, True)
    if exist_ok and path.isdir(target):
        return None
    return _fs.mkdir(target)


def rmdir(target):
    """Removes the EMPTY directory `target`."""
    return _fs.rmdir(target)


def rename(src, dst):
    """Renames `src` to `dst`."""
    return _fs.rename(src, dst)


replace = rename


def stat(target):
    raise NotImplementedError(
        "os.stat() needs a stat_result this runtime does not carry; use os.path.getsize(), "
        "os.path.isfile() and os.path.isdir()"
    )


def getcwd():
    raise NotImplementedError(
        "os.getcwd() needs a working directory this runtime does not own; pass paths explicitly"
    )


def chdir(target):
    raise NotImplementedError(
        "os.chdir() needs a working directory this runtime does not own; pass paths explicitly"
    )


def walk(top, topdown=True, onerror=None, followlinks=False):
    raise NotImplementedError(
        "os.walk() is not implemented; os.listdir() plus os.path.isdir() builds one"
    )


class _Path:
    """The `os.path` surface: path arithmetic, and the predicates over `_fs.kind`."""

    def join(self, first, *rest):
        joined = first
        for part in rest:
            if part.startswith(sep):
                joined = part
            elif joined == "" or joined.endswith(sep):
                joined = joined + part
            else:
                joined = joined + sep + part
        return joined

    def split(self, target):
        at = target.rfind(sep)
        if at < 0:
            return ("", target)
        head = target[:at]
        if head == "":
            head = sep
        return (head, target[at + 1:])

    def dirname(self, target):
        return self.split(target)[0]

    def basename(self, target):
        return self.split(target)[1]

    def splitext(self, target):
        base = self.basename(target)
        at = base.rfind(extsep)
        # A leading dot is part of the name, not an extension: `.config` has none.
        if at <= 0:
            return (target, "")
        cut = len(target) - (len(base) - at)
        return (target[:cut], target[cut:])

    def isabs(self, target):
        return target.startswith(sep)

    def normpath(self, target):
        absolute = self.isabs(target)
        parts = []
        for part in target.split(sep):
            if part == "" or part == ".":
                continue
            if part == "..":
                if parts and parts[-1] != "..":
                    parts.pop()
                elif not absolute:
                    parts.append(part)
                continue
            parts.append(part)
        joined = sep.join(parts)
        if absolute:
            return sep + joined
        return joined if joined else "."

    def _kind(self, target):
        # `(is_directory, size)`, or None when the path is not there. The refusal is CAUGHT here
        # rather than avoided, because there is no way to ask without asking.
        try:
            return _fs.kind(target)
        except OSError:
            return None

    def exists(self, target):
        return self._kind(target) is not None

    def isfile(self, target):
        found = self._kind(target)
        return found is not None and not found[0]

    def isdir(self, target):
        found = self._kind(target)
        return found is not None and found[0]

    def getsize(self, target):
        found = _fs.kind(target)
        return found[1]

    def abspath(self, target):
        raise NotImplementedError(
            "os.path.abspath() needs a working directory this runtime does not own"
        )


path = _Path()
