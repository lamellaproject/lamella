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
define="LAMELLA_SURFACE_FLOAT;LAMELLA_SURFACE_MATH_TRANSCENDENTAL;LAMELLA_SURFACE_GC;LAMELLA_SURFACE_VARARGS;LAMELLA_SURFACE_TYPED_REFERENCES;LAMELLA_SURFACE_DECIMAL;LAMELLA_SURFACE_THREADS;LAMELLA_SURFACE_WAIT_HANDLES;LAMELLA_SURFACE_NET;LAMELLA_SURFACE_NET_TLS;LAMELLA_NET_2_0;LAMELLA_SURFACE_NETFX_1_1;LAMELLA_SURFACE_NETFX_2_0;LAMELLA_SURFACE_NETFX_4_0;LAMELLA_SURFACE_FILE_IO;LAMELLA_SURFACE_SERIAL;LAMELLA_SURFACE_STRING_COMPARISON;LAMELLA_SURFACE_REFLECTION"

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
assemblies=(
    "Lamella.Hardware|"
    "System.Device.Gpio|"
    "System.Device.Model|"
    "System.Device.Pwm|"
    "System.Net.NetworkInformation|"
    "Lamella.Net.Time|"
    "Lamella.Net.Time.Nts|Lamella.Net.Time"
    "Lamella.IO.Storage|System.Device.Gpio"
    "nanoFramework.System.Device.Adc|"
    "nanoFramework.System.IO.FileSystem|"
)

# An assembly present on disk but missing from the list above would be silently skipped, and a
# library that nothing compiles is how a source file rots undetected. Fail instead.
listed="$(for a in "${assemblies[@]}"; do printf '%s\n' "${a%%|*}"; done | LC_ALL=C sort)"
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

# The language version follows the capability surface -- the same rule, condition and rung as
# build-managed.ps1, which compiles these same sources and states the reasoning in full.
# `corlib/System/Collections/Generic/*.cs` is guarded by LAMELLA_SURFACE_NETFX_2_0, so that symbol is
# what brings generic sources into the compile, and at C# 1.0 they are refused with CS8022 rather
# than passing by unused.
#
# Matched with the separators around it, so a symbol that merely ends in one of these names cannot
# match.
case ";$define;" in
    *";LAMELLA_SURFACE_NETFX_2_0;"*|*";LAMELLA_SURFACE_GENERICS;"*) langversion="--langversion=2" ;;
    *) langversion="--langversion=1" ;;
esac

# --- corlib ------------------------------------------------------------------------------------
corlib_dll="$out/corlib.dll"
mapfile -t corlib_src < <(sources_of "$root/corlib" "")
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
    name="${entry%%|*}"
    deps="${entry#*|}"
    mapfile -t src < <(sources_of "$root/libs/$name" 1)
    if [ "${#src[@]}" -eq 0 ]; then echo "libs/$name contains no .cs sources" >&2; exit 1; fi
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
