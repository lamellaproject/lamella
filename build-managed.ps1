# Builds Lamella's managed (C#) sources into the assemblies the runtime loads.
#
#   pwsh -File build-managed.ps1 [-OutDir <dir>] [-Lcsc <path>] [-Define <symbols>]
#
# The Rust side is `cargo build`; this is the other half. It compiles `corlib/` and every
# assembly under `libs/` with `lcsc` (this repository's C# compiler, built from `crates/lcsc`),
# in dependency order, and writes them to -OutDir (default `managed/`). Nothing else in the tree
# is read or written.
#
# The sources alone are not enough to reproduce these assemblies: the corlib needs an exact set of
# capability symbols, and the libraries have a reference order that is not derivable from the files.
# Both are encoded here.
#
# The output is order-stable by design. A compiler lays metadata out in the order its sources
# arrive, and directory enumeration order differs between operating systems and even between
# PowerShell hosts -- so the file list is SORTED before it is passed. Without that, the same
# sources on two machines produce assemblies of identical length and entirely different bytes.

[CmdletBinding()]
param(
    [string]$OutDir = 'managed',
    [string]$Lcsc,
    [string[]]$Define
)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot

# The capability surface the assemblies are compiled at: every capability this runtime implements.
# Each name gates `#if` regions in the sources. A smaller set builds a smaller BCL -- but it must
# match the cargo features the runtime is built with, because a method whose intrinsic was compiled
# out keeps its placeholder body and silently returns zero rather than failing to load.
$DefaultSurface = @(
    'LAMELLA_SURFACE_FLOAT',
    'LAMELLA_SURFACE_MATH_TRANSCENDENTAL',
    'LAMELLA_SURFACE_GC',
    'LAMELLA_SURFACE_VARARGS',
    'LAMELLA_SURFACE_TYPED_REFERENCES',
    'LAMELLA_SURFACE_DECIMAL',
    'LAMELLA_SURFACE_THREADS',
    'LAMELLA_SURFACE_WAIT_HANDLES',
    'LAMELLA_SURFACE_NET',
    'LAMELLA_SURFACE_NET_TLS',
    'LAMELLA_NET_2_0',
    'LAMELLA_SURFACE_NETFX_1_1',
    'LAMELLA_SURFACE_NETFX_2_0',
    'LAMELLA_SURFACE_NETFX_4_0',
    'LAMELLA_SURFACE_NETFX_4_5',
    'LAMELLA_SURFACE_FILE_IO',
    'LAMELLA_SURFACE_SERIAL',
    'LAMELLA_SURFACE_STRING_COMPARISON',
    'LAMELLA_SURFACE_REFLECTION'
)
if (-not $Define) { $Define = $DefaultSurface }

# Every assembly under libs/, in an order where each is built after what it references. `references`
# names other entries in this list; all of them reference the corlib implicitly.
$Assemblies = @(
    @{ name = 'Lamella.Hardware';                      references = @() },
    @{ name = 'System.Device.Gpio';                    references = @() },
    @{ name = 'System.Device.Model';                   references = @() },
    @{ name = 'System.Device.Pwm';                     references = @() },
    @{ name = 'System.Net.NetworkInformation';         references = @() },
    # Real .NET's own assembly name, bare and unprefixed, because these are real .NET's types in real
    # .NET's namespace: it ships System.IO.Ports out-of-band (dotnet/dotnet) rather than in its
    # corlib, NETMF v4.4 has no SerialPort in mscorlib at all, and nanoFramework has it as a separate
    # assembly of this same name. All three agree it does not belong in a core library, which is why
    # it moved here. References only corlib -- these sources carry no `using` directives.
    @{ name = 'System.IO.Ports';                       references = @() },
    @{ name = 'Lamella.Net.Time';                      references = @() },
    @{ name = 'Lamella.Net.Time.Nts';                  references = @('Lamella.Net.Time') },
    @{ name = 'Lamella.IO.Storage';                    references = @('System.Device.Gpio') },
    # The `nanoFramework.` prefix marks a compatibility surface: the types keep nanoFramework's
    # namespaces so unmodified nanoFramework source compiles, and the ASSEMBLY name is what says
    # this is not the authoritative .NET design.
    # It references Lamella.Hardware and not the other way round: the AdcDriver seam and the
    # AdcControllers binding ship from there, so a program reaching an ADC needs no compatibility
    # assembly. This assembly holds the nanoFramework-shaped surface over that seam.
    @{ name = 'nanoFramework.System.Device.Adc';       references = @('Lamella.Hardware') },
    @{ name = 'nanoFramework.System.IO.FileSystem';    references = @() }
)

# An assembly present on disk but missing from the list above would be silently skipped, and a
# library that nothing compiles is how a source file rots undetected. Fail instead.
$onDisk = @(Get-ChildItem (Join-Path $root 'libs') -Directory |
    Where-Object { $_.Name -ne 'checks' } | ForEach-Object { $_.Name } | Sort-Object)
$listed = @($Assemblies | ForEach-Object { $_.name } | Sort-Object)
$unlisted = @($onDisk | Where-Object { $listed -notcontains $_ })
$missing = @($listed | Where-Object { $onDisk -notcontains $_ })
if ($unlisted.Count) { throw "libs/ contains assemblies this script does not build: $($unlisted -join ', '). Add them to `$Assemblies (in dependency order)." }
if ($missing.Count) { throw "this script lists assemblies that are not in libs/: $($missing -join ', ')." }

# --- Locate the compiler -----------------------------------------------------------------------
if (-not $Lcsc) {
    Write-Host 'Building lcsc (cargo build --release -p lcsc)...'
    & cargo build --release -p lcsc
    if ($LASTEXITCODE -ne 0) { throw "cargo build -p lcsc failed ($LASTEXITCODE)" }
    $targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
    $Lcsc = Join-Path $targetDir 'release/lcsc.exe'
    if (-not (Test-Path $Lcsc)) { $Lcsc = Join-Path $targetDir 'release/lcsc' }
}
if (-not (Test-Path $Lcsc)) {
    throw "lcsc not found at '$Lcsc'. Build it with ``cargo build --release -p lcsc`` and pass -Lcsc <path>."
}

$out = if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $root $OutDir }
New-Item -ItemType Directory -Force -Path $out | Out-Null
$defineArg = @("/define:$($Define -join ';')")

# The language version follows the capability surface, and it is stated here rather than left to a
# default. `corlib/System/Collections/Generic/*.cs` is guarded by LAMELLA_SURFACE_NETFX_2_0, so that
# symbol is what brings generic sources into the compile -- and at C# 1.0 they are refused with
# CS8022 rather than passing by unused. LAMELLA_SURFACE_GENERICS selects the same rung, for a caller
# who names it directly.
#
# The C# 1.0 branch is written out rather than left implicit. A surface with no generic sources in it
# is compiled as C# 1.0 and refuses a 2.0 construct anywhere in these sources, which is what keeps a
# 1.0 build honest; passing no version at all would instead inherit whatever the compiler's own
# default is, and that default is the newest language version it supports.
$langArg = @('/langversion:1')
if ($Define -contains 'LAMELLA_SURFACE_NETFX_2_0' -or $Define -contains 'LAMELLA_SURFACE_GENERICS') {
    $langArg = @('/langversion:2')
}

# Sorted, so the emitted metadata does not depend on the filesystem's enumeration order.
#
# EVERY CALLER MUST WRAP THIS IN @(). PowerShell unrolls a function's output onto the pipeline, so a
# single-source assembly comes back as a bare STRING no matter what this function does internally --
# and a bare string splats one CHARACTER at a time into the compiler's argument list
# (`lcsc: cannot read 'E'`). The array-subexpression has to be at the assignment, which is why it is
# not hidden in here. No assembly under libs/ has one source file today; the wrapping stays because
# the next one added might.
function Get-Sources($dir, [switch]$Recurse) {
    $files = if ($Recurse) { Get-ChildItem $dir -Filter *.cs -Recurse } else { Get-ChildItem $dir -Filter *.cs }
    $full = @($files | ForEach-Object { $_.FullName })
    # Sort on the path RELATIVE to $dir with separators normalized to '/', by ORDINAL comparison.
    # All three parts matter for the sort to agree with build-managed.sh: an absolute prefix differs
    # per machine, '\' (0x5C) and '/' (0x2F) fall on opposite sides of the letters so a raw path sorts
    # differently on Windows than on Unix, and PowerShell's default Sort-Object is culture-aware where
    # `LC_ALL=C sort` is byte order.
    $prefix = $dir.TrimEnd('\', '/').Length + 1
    $keys = @($full | ForEach-Object { $_.Substring($prefix).Replace('\', '/') })
    $paths = @($full)
    [Array]::Sort($keys, $paths, [System.StringComparer]::Ordinal)
    $paths
}

# --- corlib ------------------------------------------------------------------------------------
$corlibDll = Join-Path $out 'corlib.dll'
$corlibSrc = @(Get-Sources (Join-Path $root 'corlib') -Recurse)
Write-Host "corlib ($($corlibSrc.Count) sources) -> $corlibDll"
# /unsafe: the corlib carries unsafe source (String's char* constructors and its pinnable
# reference). None of the libs/ assemblies do, so the flag stays on the one compilation that
# needs it rather than becoming a blanket -- the point of an opt-in is that it is scoped.
& $Lcsc @corlibSrc @langArg @defineArg /unsafe "/out:$corlibDll" /debug-
if ($LASTEXITCODE -ne 0) { throw "corlib compile failed ($LASTEXITCODE)" }

# --- libs --------------------------------------------------------------------------------------
foreach ($assembly in $Assemblies) {
    $name = $assembly.name
    $src = @(Get-Sources (Join-Path $root "libs/$name"))
    if (-not $src.Count) { throw "libs/$name contains no .cs sources" }
    $dll = Join-Path $out "$name.dll"
    $refs = @("/reference:$corlibDll")
    foreach ($r in $assembly.references) { $refs += "/reference:$(Join-Path $out "$r.dll")" }
    Write-Host "$name ($($src.Count) sources) -> $dll"
    & $Lcsc @src @langArg @defineArg @refs /target:library "/out:$dll" /debug-
    if ($LASTEXITCODE -ne 0) { throw "$name compile failed ($LASTEXITCODE)" }
}

Write-Host ''
foreach ($f in (Get-ChildItem $out -Filter *.dll | Sort-Object Name)) {
    Write-Host ("  {0,-34} {1,9:N0} bytes" -f $f.Name, $f.Length)
}
Write-Host ''
Write-Host "Done. $((Get-ChildItem $out -Filter *.dll).Count) assemblies in $out"
