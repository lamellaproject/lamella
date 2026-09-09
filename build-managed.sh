#!/usr/bin/env bash
# Builds Lamella's managed (C#) sources into the assemblies the runtime loads.
#
#   ./build-managed.sh [--out-dir <dir>] [--lcsc <path>] [--define <sym,sym,...>]
#
# The Rust side is `cargo build`; this is the other half. It compiles `corlib/` and every assembly
# under `libs/` with `lcsc` (this repository's C# compiler, built from `crates/lcsc`), in dependency
# order, and writes them to --out-dir (default `managed/`). Nothing else in the tree is read or
# written. `build-managed.ps1` is the same build for Windows and produces the same bytes.
#
# The sources alone are not enough to reproduce these assemblies: the corlib needs an exact set of
# capability symbols, and the libraries have a reference order that is not derivable from the files.
# Both are encoded here.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out_dir="managed"
lcsc=""

# The capability surface the assemblies are compiled at: every capability this runtime implements.
# Each name gates `#if` regions in the sources. A smaller set builds a smaller BCL -- but it must
# match the cargo features the runtime is built with, because a method whose intrinsic was compiled
# out keeps its placeholder body and silently returns zero rather than failing to load.
#
# LAMELLA_SURFACE_SPAN is what brings `corlib/System/Span.cs` into the compile. `System.Device.Gpio`'s
# I2C and SPI facades take `Span<byte>` and `ReadOnlySpan<byte>` -- Microsoft's signatures for those
# members -- so without it those facades name a type that does not exist and that assembly cannot be
# built at all. It also selects the language version below.
define="LAMELLA_SURFACE_FLOAT;LAMELLA_SURFACE_MATH_TRANSCENDENTAL;LAMELLA_SURFACE_GC;LAMELLA_SURFACE_VARARGS;LAMELLA_SURFACE_TYPED_REFERENCES;LAMELLA_SURFACE_DECIMAL;LAMELLA_SURFACE_THREADS;LAMELLA_SURFACE_WAIT_HANDLES;LAMELLA_SURFACE_NET;LAMELLA_SURFACE_NET_TLS;LAMELLA_NET_2_0;LAMELLA_SURFACE_GENERICS;LAMELLA_SURFACE_NETFX_1_1;LAMELLA_SURFACE_NETFX_2_0;LAMELLA_SURFACE_NETFX_4_0;LAMELLA_SURFACE_NETFX_4_5;LAMELLA_SURFACE_FILE_IO;LAMELLA_SURFACE_SERIAL;LAMELLA_SURFACE_STRING_COMPARISON;LAMELLA_SURFACE_REFLECTION;LAMELLA_SURFACE_SPAN"

while [ $# -gt 0 ]; do
    case "$1" in
        --out-dir) out_dir="$2"; shift 2 ;;
        --lcsc)    lcsc="$2"; shift 2 ;;
        --define)  define="$(printf '%s' "$2" | tr ',' ';')"; shift 2 ;;
        -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

# Every assembly under libs/, in an order where each is built after what it references. The second
# field lists other entries here that it references; all of them reference the corlib implicitly.
# The `nanoFramework.` prefix marks a compatibility surface: the types keep nanoFramework's
# namespaces so unmodified nanoFramework source compiles, and the ASSEMBLY name is what says this is
# not the authoritative .NET design.
# Fields are `name|references|foldedIn`, and an empty field is meaningful: `System.Device.Gpio||…`
# references nothing beyond the corlib. `foldedIn` names source directories that build INTO this
# assembly instead of getting one of their own -- upstream ships `PwmChannel` inside
# `System.Device.Gpio.dll`, so the namespace is unchanged and only the assembly is. A folded
# directory must not also be listed here on its own, or the same type lands in two assemblies and a
# reference resolves it from whichever is found first.
assemblies=(
    "Lamella.Hardware|"
    "System.Device.Gpio||System.Device.Pwm"
    "System.Device.Model|"
    "System.Net.NetworkInformation|"
    # Bare, unprefixed: real .NET's own assembly name for real .NET's types. See build-managed.ps1's
    # entry for the three-way argument. References only corlib.
    "System.IO.Ports|"
    "Lamella.Net.Time|"
    "Lamella.Net.Time.Nts|Lamella.Net.Time"
    "Lamella.IO.Storage|System.Device.Gpio"
    "nanoFramework.System.Device.Adc|Lamella.Hardware"
    "nanoFramework.System.IO.FileSystem|"
    # The NETMF compatibility tier, so an existing Microsoft.SPOT.Hardware program compiles
    # unchanged. A fourth field holds a row back and names the reason; a held row is reported on
    # every run rather than skipped quietly.
    "Microsoft.SPOT.Hardware|System.Device.Gpio||nested-type identity"
)

# An assembly present on disk but missing from the list above would be silently skipped, and a
# library that nothing compiles is how a source file rots undetected. Fail instead.
#
# A FOLDED directory counts as listed. Its files ARE compiled -- into the assembly that folds them --
# so the rot this guard exists to catch cannot reach them, and reading only the name would make the
# guard fire on a directory the moment it stopped having an assembly of its own.
listed="$(for a in "${assemblies[@]}"; do
    IFS='|' read -r n _ f _ <<< "$a"
    printf '%s\n' "$n"
    if [ -n "$f" ]; then printf '%s\n' "$f" | tr ',' '\n'; fi
done | LC_ALL=C sort)"
on_disk="$(find "$root/libs" -mindepth 1 -maxdepth 1 -type d ! -name checks -exec basename {} \; | LC_ALL=C sort)"
if [ "$listed" != "$on_disk" ]; then
    echo "libs/ and this script's assembly list disagree. Add new assemblies to \`assemblies\` (in dependency order)." >&2
    diff <(printf '%s\n' "$listed") <(printf '%s\n' "$on_disk") >&2 || true
    exit 1
fi

# --- Locate the compiler -----------------------------------------------------------------------
if [ -z "$lcsc" ]; then
    echo "Building lcsc (cargo build --release -p lcsc)..."
    (cd "$root" && cargo build --release -p lcsc)
    lcsc="${CARGO_TARGET_DIR:-$root/target}/release/lcsc"
fi
if [ ! -x "$lcsc" ]; then
    echo "lcsc not found at '$lcsc'. Build it with \`cargo build --release -p lcsc\` and pass --lcsc <path>." >&2
    exit 1
fi

case "$out_dir" in /*) out="$out_dir" ;; *) out="$root/$out_dir" ;; esac
mkdir -p "$out"

# Sorted on the path RELATIVE to the directory, by byte order, so the emitted metadata does not
# depend on the filesystem's enumeration order -- and matches build-managed.ps1, which sorts the
# same normalized key ordinally rather than by culture.
sources_of() {
    local dir="$1" depth="$2"
    (cd "$dir" && find . -mindepth 1 ${depth:+-maxdepth 1} -name '*.cs' | sed 's|^\./||' | LC_ALL=C sort |
        while IFS= read -r f; do printf '%s/%s\n' "$dir" "$f"; done)
}

# Reads stdin into REPLY_LINES, one element per line, replacing `mapfile`.
#
# `mapfile` is a bash 4 builtin and macOS ships bash 3.2 at /bin/bash, where it does not exist at
# all -- so a script using it fails there with `command not found` rather than with anything naming
# the version. This stands in for it in one place rather than three open-coded loops, because three
# copies of a rule is how one of them comes to differ.
#
# It answers through a global because bash 3.2 has no namerefs to assign a caller's array by name,
# and the caller copies out of it immediately. `IFS=` and `read -r` keep leading whitespace and
# backslashes, which are legal in a path.
read_lines() {
    REPLY_LINES=()
    local line
    while IFS= read -r line; do REPLY_LINES+=("$line"); done
}

# The language version follows the capability surface -- the same rule, condition and rung as
# build-managed.ps1, which compiles these same sources and states the reasoning in full.
# `corlib/System/Collections/Generic/*.cs` is guarded by LAMELLA_SURFACE_NETFX_2_0, so that symbol is
# what brings generic sources into the compile, and at C# 1.0 they are refused with CS8022 rather
# than passing by unused.
#
# Matched with the separators around it, so a symbol that merely ends in one of these names cannot
# match.
#
# Highest rung wins, so SPAN is tested first. `Span<T>` is a `readonly ref struct` with
# byref-returning members -- C# 7.2 constructs, refused at 2 with CS8023 -- so its symbol and its
# language version have to move together. Without the symbol the lower rung still stands, so a
# surface that does not offer spans keeps refusing a 7.2 construct anywhere in these sources.
case ";$define;" in
    *";LAMELLA_SURFACE_SPAN;"*) langversion="--langversion=7.2" ;;
    *";LAMELLA_SURFACE_NETFX_2_0;"*|*";LAMELLA_SURFACE_GENERICS;"*) langversion="--langversion=2" ;;
    *) langversion="--langversion=1" ;;
esac

# --- corlib ------------------------------------------------------------------------------------
corlib_dll="$out/corlib.dll"
read_lines < <(sources_of "$root/corlib" "")
corlib_src=()
# Guarded rather than expanded straight: bash 3.2 under `set -u` treats an EMPTY array
# expansion as an unbound variable, so `"${REPLY_LINES[@]}"` on no sources would abort here
# with a message about the wrong thing.
if [ ${#REPLY_LINES[@]} -gt 0 ]; then corlib_src=("${REPLY_LINES[@]}"); fi
echo "corlib (${#corlib_src[@]} sources) -> $corlib_dll"
# The `--switch=value` spellings, not csc's `/switch:value`: MSYS/Git Bash rewrites an argument that
# begins with `/` into a Windows path, and lcsc would then read the mangled result as a SOURCE FILE.
# `--out=`, `--reference=`, `--define=` and `--no-debug` are lcsc's documented equivalents. There is
# deliberately no `/target:` here -- lcsc ignores it and infers the output kind from whether a `Main`
# is present, so passing it would only be a spelling that can break.
# --unsafe: the corlib carries unsafe source (String's char* constructors and its pinnable
# reference). None of the libs/ assemblies do, so the flag stays on the one compilation that needs
# it rather than becoming a blanket -- the point of an opt-in is that it is scoped.
"$lcsc" "${corlib_src[@]}" "$langversion" "--define=$define" --unsafe "--out=$corlib_dll" --no-debug

# --- libs --------------------------------------------------------------------------------------
for entry in "${assemblies[@]}"; do
    IFS='|' read -r name deps folded held <<< "$entry"
    if [ -n "$held" ]; then
        echo "$name -- HELD, not built: $held"
        continue
    fi
    read_lines < <(sources_of "$root/libs/$name" 1)
    src=()
    if [ ${#REPLY_LINES[@]} -gt 0 ]; then src=("${REPLY_LINES[@]}"); fi
    if [ "${#src[@]}" -eq 0 ]; then echo "libs/$name contains no .cs sources" >&2; exit 1; fi
    if [ -n "$folded" ]; then
        IFS=',' read -ra folded_list <<< "$folded"
        for f in "${folded_list[@]}"; do
            read_lines < <(sources_of "$root/libs/$f" 1)
            extra_src=()
            if [ ${#REPLY_LINES[@]} -gt 0 ]; then extra_src=("${REPLY_LINES[@]}"); fi
            if [ "${#extra_src[@]}" -eq 0 ]; then echo "libs/$f contains no .cs sources" >&2; exit 1; fi
            src+=("${extra_src[@]}")
        done
    fi
    refs=("--reference=$corlib_dll")
    if [ -n "$deps" ]; then
        IFS=',' read -ra dep_list <<< "$deps"
        for d in "${dep_list[@]}"; do refs+=("--reference=$out/$d.dll"); done
    fi
    echo "$name (${#src[@]} sources) -> $out/$name.dll"
    "$lcsc" "${src[@]}" "$langversion" "--define=$define" "${refs[@]}" "--out=$out/$name.dll" --no-debug
done

echo
for f in "$out"/*.dll; do printf '  %-38s %9d bytes\n' "$(basename "$f")" "$(wc -c < "$f")"; done
echo
echo "Done. $(ls -1 "$out"/*.dll | wc -l) assemblies in $out"
