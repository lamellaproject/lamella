# lamella, the root package for everything this project provides that CPython does not.
#
# It has no surface of its own on purpose. `import lamella.clock` imports this body first (a package
# is imported before the module hung on it), so anything added here is paid for by every submodule
# any program imports.
#
# Nothing of ours goes into CPython's own module names; nothing of CPython's is re-exported from
# here. A reader who sees `lamella.` knows the name is ours without having to check.
