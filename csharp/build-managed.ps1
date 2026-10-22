# Builds Lamella's managed (C#) sources into the assemblies the runtime loads.
#
#   pwsh -File csharp/build-managed.ps1 [-OutDir <dir>] [-Lcsc <path>] [-Define <symbols>]
#   pwsh -File csharp/build-managed.ps1 -PrintSurface        # the default symbols, one per line
#   pwsh -File csharp/build-managed.ps1 -PrintFamilySources <family>   # the csp sources that family's
#                                                     # System.Device.Gpio carries, one per line
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
    [string[]]$Define,
    # Print the default surface and exit, so anything that needs to know what a plain build compiles
    # asks this script instead of pattern-matching its text. A reader of the text cannot tell a
    # member of the list from a symbol named anywhere else in the file.
    [switch]$PrintSurface,
    # Print the `csp/<family>/csharp` sources the family's System.Device.Gpio carries and exit, so
    # csharp/build-boards.ps1 leaves out of a board's assembly exactly what this script compiles into that
    # build: one reader of the list serves both scripts.
    [string]$PrintFamilySources
)

$ErrorActionPreference = 'Stop'
# The repository root: this script is in its csharp/ folder, and every path it reads is the root's.
$root = Split-Path $PSScriptRoot -Parent

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
    'LAMELLA_SURFACE_GENERICS',
    'LAMELLA_SURFACE_NETFX_1_1',
    'LAMELLA_SURFACE_NETFX_2_0',
    'LAMELLA_SURFACE_NETFX_4_0',
    'LAMELLA_SURFACE_NETFX_4_5',
    # The rung above 4.5: members .NET Core 2.0 introduced, each gated where it was introduced, like
    # the four eras below it. It takes no language version of its own.
    'LAMELLA_SURFACE_NETCORE_2_0',
    # The rung above it: what .NET Core 2.1 introduced, such as System.Buffers.Binary.BinaryPrimitives.
    # Every .NET Core 2.1 shipped with spans, so it requires LAMELLA_SURFACE_SPAN as well.
    'LAMELLA_SURFACE_NETCORE_2_1',
    # And above that, what .NET Core 3.0 introduced, such as MidpointRounding's directed modes.
    'LAMELLA_SURFACE_NETCORE_3_0',
    # The rung above that: what .NET 5.0 introduced, such as X509Certificate2.CreateFromPem. It stands
    # on .NET Core 3.0, the rung below it.
    'LAMELLA_SURFACE_NET_5_0',
    'LAMELLA_SURFACE_FILE_IO',
    'LAMELLA_SURFACE_SERIAL',
    'LAMELLA_SURFACE_STRING_COMPARISON',
    'LAMELLA_SURFACE_REFLECTION',
    # `System.Device.Gpio`'s I2C and SPI facades take `Span<byte>` and `ReadOnlySpan<byte>`, which
    # are Microsoft's signatures for those members. The symbol brings `corlib/System/Span.cs` into
    # the compile; without it those facades name a type that does not exist and the assembly listed
    # below cannot be built at all.
    'LAMELLA_SURFACE_SPAN',
    # Tuple syntax names `System.ValueTuple<...>` and nothing else, so the symbol that brings
    # `corlib/System/ValueTuple.cs` into the compile is what makes `(int, string)` compilable at all.
    # Without it a tuple type resolves to no type and the use site reports a refused object creation.
    'LAMELLA_SURFACE_TUPLES'
)
if ($PrintSurface) { $DefaultSurface | ForEach-Object { Write-Output $_ }; exit 0 }

# The `csp/<family>/csharp` sources `libs/System.Device.Gpio/Families/<family>.sources` lists, as
# paths relative to that directory with '/' separators: what the family's System.Device.Gpio carries.
# A blank line or one that starts with '#' is not an entry. An entry that names no source there is
# refused, so a source renamed or removed cannot leave the family build without a type it was meant to
# carry.
function Get-FamilySources([string]$family) {
    $list = Join-Path $root "libs/System.Device.Gpio/Families/$family.sources"
    if (-not (Test-Path -LiteralPath $list)) {
        throw "libs/System.Device.Gpio/Families/$family.sources does not exist: it lists the csp/$family/csharp sources the $family build of System.Device.Gpio carries"
    }
    $entries = @()
    foreach ($line in [IO.File]::ReadAllLines($list)) {
        $entry = $line.Trim()
        if (-not $entry -or $entry.StartsWith('#')) { continue }
        if (-not $entry.EndsWith('.cs') -or -not (Test-Path -LiteralPath (Join-Path $root "csp/$family/csharp/$entry") -PathType Leaf)) {
            throw "libs/System.Device.Gpio/Families/$family.sources lists '$entry', which is not a C# source in csp/$family/csharp"
        }
        $entries += $entry
    }
    if (-not $entries.Count) { throw "libs/System.Device.Gpio/Families/$family.sources lists no sources" }
    $entries
}
if ($PrintFamilySources) { Get-FamilySources $PrintFamilySources | ForEach-Object { Write-Output $_ }; exit 0 }
if (-not $Define) { $Define = $DefaultSurface }

# A capability that cannot stand on its own names what it rests on. A surface offering the members
# without the capability behind them describes an image nobody can build, so it is refused here
# rather than emerging as a missing type several assemblies later. The sources carry the same pairs
# in their own `#if`, so the rule holds for any build path; this turns "you get an empty assembly"
# into "you get told why".
$SurfaceRequires = [ordered]@{
    'LAMELLA_SURFACE_NETFX_2_0' = @('LAMELLA_SURFACE_GENERICS')
    'LAMELLA_SURFACE_SERIAL'    = @('LAMELLA_SURFACE_NETFX_2_0')
    'LAMELLA_SURFACE_SPAN'      = @('LAMELLA_SURFACE_GENERICS')
    'LAMELLA_SURFACE_TUPLES'    = @('LAMELLA_SURFACE_GENERICS')
    # A rung stands on the one below it: .NET Core 2.0 members without the 4.5 surface describe no
    # .NET that ever shipped.
    'LAMELLA_SURFACE_NETCORE_2_0' = @('LAMELLA_SURFACE_NETFX_4_5')
    # And .NET Core 2.1 without spans describes none either.
    'LAMELLA_SURFACE_NETCORE_2_1' = @('LAMELLA_SURFACE_NETCORE_2_0', 'LAMELLA_SURFACE_SPAN')
    'LAMELLA_SURFACE_NETCORE_3_0' = @('LAMELLA_SURFACE_NETCORE_2_1')
    # And .NET 5.0 without the .NET Core rungs below it describes none.
    'LAMELLA_SURFACE_NET_5_0' = @('LAMELLA_SURFACE_NETCORE_3_0')
}
foreach ($symbol in $SurfaceRequires.Keys) {
    if ($Define -notcontains $symbol) { continue }
    foreach ($required in $SurfaceRequires[$symbol]) {
        if ($Define -notcontains $required) {
            throw "$symbol requires $required. Add $required to -Define, or drop $symbol."
        }
    }
}

# Every assembly under libs/, in an order where each is built after what it references. `references`
# names other entries in this list; all of them reference the corlib implicitly. A row may carry
# `held = '<reason>'`, and is then reported on every run instead of built.
$Assemblies = @(
    # Its own assembly, and the contrast with `System.Device.Pwm`'s fold below is the whole reason. Upstream's
    # `System.Device.Gpio` holds Gpio, I2c, Pwm and Spi and no Analog, so folding `System.Device.Pwm`
    # converges on that shape and folding Analog would be a deviation dressed as convergence.
    # References only corlib: these sources name `System.IDisposable` and nothing else.
    @{ name = 'System.Device.Analog';                  references = @() },
    # It references System.Device.Analog and not the other way round: the analog controller it offers
    # over the ADC seam is a System.Device.Analog.AnalogController, and the dotnet/iot-shaped surface
    # a portable program references depends on nothing of ours.
    @{ name = 'Lamella.Hardware';                      references = @('System.Device.Analog') },
    # `System.Device.Pwm`'s sources build INTO this assembly and it gets no assembly of its own:
    # upstream ships `PwmChannel` inside `System.Device.Gpio.dll`. The namespace is unchanged;
    # only the assembly is.
    @{ name = 'System.Device.Gpio';                    references = @(); extraSources = @('System.Device.Pwm') },
    @{ name = 'System.Device.Model';                   references = @() },
    @{ name = 'System.Net.NetworkInformation';         references = @() },
    # Full .NET's own assembly name, bare and unprefixed, because these are full .NET's types in full
    # .NET's namespace: it ships System.IO.Ports out-of-band (dotnet/dotnet) rather than in its
    # corlib, NETMF v4.4 has no SerialPort in mscorlib at all, and nanoFramework has it as a separate
    # assembly of this same name. All three agree it does not belong in a core library, which is why
    # it moved here. References only corlib -- these sources carry no `using` directives.
    @{ name = 'System.IO.Ports';                       references = @() },
    @{ name = 'Lamella.Net.Time';                      references = @() },
    @{ name = 'Lamella.Net.Time.Nts';                  references = @('Lamella.Net.Time') },
    # The board's Wi-Fi radio. It references System.Net.NetworkInformation because the addresses a
    # board holds on the network are that assembly's report, which WiFiAdapter.WaitForAddress reads.
    @{ name = 'Lamella.Net.WiFi';                      references = @('System.Net.NetworkInformation') },
    @{ name = 'Lamella.IO.Storage';                    references = @('System.Device.Gpio') }
)

# An assembly present on disk but missing from the list above would be silently skipped, and a
# library that nothing compiles is how a source file rots undetected. Fail instead.
#
# A FOLDED directory counts as listed. `extraSources` names a source set that builds INTO another
# assembly rather than getting one of its own, so its files ARE compiled and the rot this guard
# exists to catch cannot reach them. A folded directory must therefore be read from `extraSources`
# as well as from `name`, or the guard reports it as absent the moment it stops being an assembly.
$onDisk = @(Get-ChildItem (Join-Path $root 'libs') -Directory |
    Where-Object { $_.Name -ne 'checks' } | ForEach-Object { $_.Name } | Sort-Object)
$listed = @($Assemblies | ForEach-Object { $_.name; $_.extraSources } |
    Where-Object { $_ } | Sort-Object)
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
#
# The rungs are ordered lowest first and each one overwrites the last, so the highest capability in
# the surface decides the version. `Span<T>` is a `readonly ref struct` with byref-returning members
# -- C# 7.2 constructs, refused at 2 with CS8023 -- so its symbol and its language version have to
# move together. The property the paragraph above is about survives: WITHOUT the symbol you stay on
# the lower rung, so a surface that does not offer spans still refuses a 7.2 construct anywhere in
# these sources.
#
# LAMELLA_SURFACE_TUPLES DELIBERATELY HAS NO RUNG HERE, and the absence is the point of this note.
# This ladder exists because a capability's own SOURCES need language constructs -- Span<T> is a
# `readonly ref struct` with byref-returning members and cannot be compiled below 7.2. The eight
# ValueTuple structs are plain generics that compile at 2, so giving tuples a 7.0 step would raise
# the version every OTHER source in this tree is compiled at and buy nothing. What gates the type is
# the SYMBOL: a profile that does not name it has no ValueTuple, whatever its language version. The
# 7.0 rung belongs to tuple SYNTAX, which is a property of the program being compiled, not of these.
$langArg = @('/langversion:1')
if ($Define -contains 'LAMELLA_SURFACE_NETFX_2_0' -or $Define -contains 'LAMELLA_SURFACE_GENERICS') {
    $langArg = @('/langversion:2')
}
if ($Define -contains 'LAMELLA_SURFACE_SPAN') {
    $langArg = @('/langversion:7.2')
}

# Sorted, so the emitted metadata does not depend on the filesystem's enumeration order.
#
# EVERY CALLER MUST WRAP THIS IN @(). PowerShell unrolls a function's output onto the pipeline, so a
# single-source assembly comes back as a bare STRING no matter what this function does internally --
# and a bare string is not a list: `+` and `+=` join text onto it rather than adding sources, and a
# splat passes it one CHARACTER at a time (`lcsc: cannot read 'E'`). The array-subexpression has to
# be at the assignment, which is why it is not hidden in here. No assembly under libs/ has one
# source file today; the wrapping stays because the next one added might.
function Get-Sources($dir, [switch]$Recurse) {
    $files = if ($Recurse) { Get-ChildItem $dir -Filter *.cs -Recurse } else { Get-ChildItem $dir -Filter *.cs }
    $full = @($files | ForEach-Object { $_.FullName })
    # Sort on the path RELATIVE to $dir with separators normalized to '/', by ORDINAL comparison.
    # All three parts matter for the sort to agree with csharp/build-managed.sh: an absolute prefix differs
    # per machine, '\' (0x5C) and '/' (0x2F) fall on opposite sides of the letters so a raw path sorts
    # differently on Windows than on Unix, and PowerShell's default Sort-Object is culture-aware where
    # `LC_ALL=C sort` is byte order.
    $prefix = $dir.TrimEnd('\', '/').Length + 1
    $keys = @($full | ForEach-Object { $_.Substring($prefix).Replace('\', '/') })
    $paths = @($full)
    [Array]::Sort($keys, $paths, [System.StringComparer]::Ordinal)
    $paths
}

# Compiles $Sources with lcsc, passing the rest of the arguments as its options, and leaves lcsc's
# exit code in $LASTEXITCODE as a direct call would.
#
# The sources go in a response file, never on the command line itself. Windows refuses a command
# line longer than 32,767 characters, and every source path on it repeats the folder this tree sits
# in, so the corlib's sources alone passed that limit under a folder path of about sixty characters:
# where the tree was unpacked decided whether it built, and every source added lowered the limit.
# lcsc reads csc's `@file`. Each path is written on its own line in quotes, so a space in it stays
# inside the one argument, and a backslash before a quote or at the end is doubled, as the quoting
# rules require.
function Invoke-Lcsc([string[]]$Sources) {
    $list = [System.IO.Path]::GetTempFileName()
    try {
        $lines = @($Sources | ForEach-Object { '"' + ($_ -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"' })
        [System.IO.File]::WriteAllLines($list, [string[]]$lines, [System.Text.UTF8Encoding]::new($false))
        & $Lcsc "@$list" @args
    } finally {
        Remove-Item -LiteralPath $list -Force -ErrorAction SilentlyContinue
    }
}

# --- corlib ------------------------------------------------------------------------------------
$corlibDll = Join-Path $out 'corlib.dll'
$corlibSrc = @(Get-Sources (Join-Path $root 'corlib') -Recurse)
Write-Host "corlib ($($corlibSrc.Count) sources) -> $corlibDll"
# /unsafe: the corlib carries unsafe source (String's char* constructors and its pinnable
# reference). None of the libs/ assemblies do, so the flag stays on the one compilation that
# needs it rather than becoming a blanket -- the point of an opt-in is that it is scoped.
Invoke-Lcsc $corlibSrc @langArg @defineArg /unsafe "/out:$corlibDll" /debug-
if ($LASTEXITCODE -ne 0) { throw "corlib compile failed ($LASTEXITCODE)" }

# --- libs --------------------------------------------------------------------------------------
$built = @('corlib.dll')
foreach ($assembly in $Assemblies) {
    $name = $assembly.name
    # A held row is reported rather than skipped quietly, so the list stays complete.
    if ($assembly.held) {
        Write-Host "$name -- HELD, not built: $($assembly.held)"
        continue
    }
    $src = @(Get-Sources (Join-Path $root "libs/$name"))
    if (-not $src.Count) { throw "libs/$name contains no .cs sources" }
    # Source directories that build INTO this assembly rather than getting one of their own.
    foreach ($extra in $assembly.extraSources) {
        $extraSrc = @(Get-Sources (Join-Path $root "libs/$extra"))
        if (-not $extraSrc.Count) { throw "libs/$extra contains no .cs sources" }
        $src += $extraSrc
    }
    $dll = Join-Path $out "$name.dll"
    $refs = @("/reference:$corlibDll")
    foreach ($r in $assembly.references) { $refs += "/reference:$(Join-Path $out "$r.dll")" }
    Write-Host "$name ($($src.Count) sources) -> $dll"
    Invoke-Lcsc $src @langArg @defineArg @refs /target:library "/out:$dll" /debug-
    if ($LASTEXITCODE -ne 0) { throw "$name compile failed ($LASTEXITCODE)" }
    $built += "$name.dll"
}

# --- System.Device.Gpio, once per family -----------------------------------------------------------
# A program that only calls dotnet/iot's factories names no board class, so the drivers its buses
# need cannot come from a board's assembly: each family with a `libs/System.Device.Gpio/Families/
# <family>.cs` gets a System.Device.Gpio of its own, at `families/<family>/System.Device.Gpio.dll`,
# holding the drivers that file constructs from a board's bus table. It is built from the shared
# sources, that file (in place of BoardDrivers.cs, by LAMELLA_FAMILY_DRIVERS) and the
# `csp/<family>/csharp` sources `Families/<family>.sources` lists: those drivers and the bindings and
# register layouts they take. The build for a board takes it in place of the shared one, so it
# presents the shared one's public surface unchanged, and beside it the csp types it carries, public.
#
# EACH OF THOSE TYPES IS IN EXACTLY ONE ASSEMBLY. csharp/build-boards.ps1 leaves the listed sources out of
# the assembly for a board of the family and compiles that assembly against this build instead. The
# AOT tier names a method by its type's full name and signature, not by its assembly, and keeps every
# definition of a name the program reaches, so a type compiled into both assemblies defines each
# method the program reaches twice, and the image does not link.
$gpio = $Assemblies | Where-Object { $_.name -eq 'System.Device.Gpio' }
$gpioSrc = @(Get-Sources (Join-Path $root 'libs/System.Device.Gpio'))
foreach ($extra in $gpio.extraSources) { $gpioSrc += @(Get-Sources (Join-Path $root "libs/$extra")) }
$familyDefineArg = @("/define:$(@($Define + 'LAMELLA_FAMILY_DRIVERS') -join ';')")
$familyFiles = @(Get-Sources (Join-Path $root 'libs/System.Device.Gpio/Families'))
foreach ($familyFile in $familyFiles) {
    $family = [IO.Path]::GetFileNameWithoutExtension($familyFile)
    $csp = Join-Path $root "csp/$family/csharp"
    if (-not (Test-Path $csp)) { throw "libs/System.Device.Gpio/Families/$family.cs names a family with no csp/$family/csharp" }
    $listed = @(Get-FamilySources $family | Sort-Object -Unique)
    $work = Join-Path ([IO.Path]::GetTempPath()) "lamella-gpio-$family-$PID"
    if (Test-Path $work) { Remove-Item -Recurse -Force $work }
    $copies = @()
    # In the order Get-Sources sorts them, not the list's, so the metadata layout does not depend on
    # how the list is written.
    foreach ($file in @(Get-Sources $csp -Recurse)) {
        $relative = $file.Substring($csp.TrimEnd('\', '/').Length + 1).Replace('\', '/')
        if ($listed -notcontains $relative) { continue }
        $copy = Join-Path $work $relative
        New-Item -ItemType Directory -Force (Split-Path $copy -Parent) | Out-Null
        # Bytes in and bytes out, so the copy differs from its original only where a declaration
        # changed: csharp/build-managed.sh makes the same edit with sed, and the two outputs are compared.
        # Inside this assembly an override of GpioDriver's `protected internal` members must say so,
        # where the source says `protected`, as it must wherever it compiles outside this assembly:
        # in a test that builds a driver from its source, and in a board's assembly for a family with
        # no build of its own. Dispose(bool) is the one plain `protected` virtual the seams declare,
        # so it keeps its modifier.
        $text = [Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes($file))
        $text = [regex]::Replace($text, '(?m)^([ \t]*)protected override(?![^\n]*Dispose\()', '${1}protected internal override')
        [IO.File]::WriteAllBytes($copy, [Text.Encoding]::UTF8.GetBytes($text))
        $copies += $copy
    }
    if ($copies.Count -ne $listed.Count) {
        throw "libs/System.Device.Gpio/Families/$family.sources lists $($listed.Count) sources and csp/$family/csharp yielded $($copies.Count) of them"
    }
    $src = $gpioSrc + @($familyFile) + $copies
    $dir = Join-Path $out "families/$family"
    New-Item -ItemType Directory -Force $dir | Out-Null
    $dll = Join-Path $dir 'System.Device.Gpio.dll'
    $refs = @("/reference:$corlibDll", "/reference:$(Join-Path $out 'Lamella.Hardware.dll')")
    Write-Host "System.Device.Gpio for $family ($($src.Count) sources) -> $dll"
    Invoke-Lcsc $src @langArg @familyDefineArg @refs /target:library "/out:$dll" /debug-
    $exit = $LASTEXITCODE
    Remove-Item -Recurse -Force $work
    if ($exit -ne 0) { throw "System.Device.Gpio for $family compile failed ($exit)" }
    $built += "families/$family/System.Device.Gpio.dll"
}

# An assembly this script STOPPED producing is the one file in $out nothing overwrites, so it sits
# there at its old date presenting a surface no source in the tree still describes, so a type
# that has moved to another assembly resolves from two at once. Only names this script OWNS are removed -- $out also
# holds board assemblies written by csharp/build-boards.ps1, so a blanket clean would delete another
# script's output.
$folded = @($Assemblies | ForEach-Object { $_.extraSources } | Where-Object { $_ })
$heldOut = @($Assemblies | Where-Object { $_.held } | ForEach-Object { $_.name })
# The compatibility libraries are external and not built here, so one of their assemblies in $out is
# stale, and anything that references it by name would still find it.
$external = @('Microsoft.SPOT.Hardware', 'nanoFramework.System.Device.Adc', 'nanoFramework.System.IO.FileSystem')
foreach ($gone in @($folded + $heldOut + $external)) {
    $stale = Join-Path $out "$gone.dll"
    if (Test-Path -LiteralPath $stale) {
        Write-Host "$gone.dll -- REMOVED from $out, this script no longer produces it"
        Remove-Item -LiteralPath $stale
    }
}

Write-Host ''
foreach ($f in (Get-ChildItem $out -Filter *.dll | Sort-Object Name)) {
    Write-Host ("  {0,-34} {1,9:N0} bytes" -f $f.Name, $f.Length)
}
Write-Host ''
# What this RUN produced, not what the directory holds: the two differ whenever a previous run
# wrote something this one does not, so a count over the directory can report a stale file as a
# fresh one.
Write-Host "Done. $($built.Count) assemblies built into $out ($((Get-ChildItem $out -Filter *.dll).Count) .dll files there, board assemblies included)"
