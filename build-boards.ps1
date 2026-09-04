# Builds each board's C# support package into a REFERENCE ASSEMBLY: `Lamella.Boards.<Vendor>.<Board>.dll`.
#
#   pwsh -File build-boards.ps1 [-OutDir <dir>] [-Lcsc <path>] [-Define <symbols>] [-Board <name>]
#
# Each board's package under `bsp/<board>/csharp/` binds that board's buses to its chip's drivers.
# Compiling it into a reference assembly is what lets a program name its board once and then use the
# standard device APIs without naming a Lamella type again:
#
#     new Pico2();                                  // arms the board's driver table
#     GpioController gpio = new GpioController();   // standard dotnet/iot, no Lamella type named
#     GpioPin led = gpio.OpenPin(25, PinMode.Output);
#
# `build-managed.ps1` is the other half and runs FIRST: it builds corlib + libs/ into -OutDir, and
# this reads them back as references. Run that, then this.
#
# One assembly per board, rather than one covering all of them. A board class reaches its chip's
# drivers (`Pico2` -> `Rp2350GpioDriver`), so a single `Lamella.Boards` would pull every family's
# drivers into every image -- unaffordable on a part whose whole image slot is 128 KB. A program
# targets one board, so it references one board.
#
# Each assembly is its board plus its chip family, which is the smallest self-contained unit: the
# generated bindings and the board class come from `bsp/<board>/csharp/`, and the register layouts,
# binding descriptors and drivers they name come from `csp/<family>/csharp/`. Neither half compiles
# alone.

[CmdletBinding()]
param(
    [string]$OutDir = 'managed',
    [string]$Lcsc,
    [string[]]$Define,
    [string]$Board
)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot

# The reference set every board assembly gets. Uniform rather than per-board: a hand-maintained
# table of which board needs the ADC surface would drift from the sources it describes, and an
# unused reference costs nothing.
$BoardReferences = @(
    'corlib',                          # every assembly
    'System.Device.Gpio',              # GpioDriver/I2cDriver/SpiDriver seams + the dotnet/iot facades
    'Lamella.Hardware',                # Mmio, and the bus table a board class binds into
    'nanoFramework.System.Device.Adc'  # AdcDriver/AdcController, for the boards that expose one
)

# --- Locate the compiler (same contract as build-managed.ps1) -----------------------------------
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
if (-not (Test-Path $out)) {
    throw "-OutDir '$out' does not exist. Run build-managed.ps1 -OutDir '$OutDir' first: this script references the assemblies it builds."
}
foreach ($r in $BoardReferences) {
    if (-not (Test-Path (Join-Path $out "$r.dll"))) {
        throw "'$r.dll' is not in '$out'. Run build-managed.ps1 -OutDir '$OutDir' first."
    }
}
$defineArg = if ($Define) { @("/define:$($Define -join ';')") } else { @() }

# Sorted by the path RELATIVE to $dir, separators normalized, ORDINAL -- so emitted metadata does
# not depend on the filesystem's enumeration order or on the host's culture. `build-managed.ps1`
# sorts identically: two scripts laying out metadata differently would produce assemblies that
# differ byte-for-byte for no reason visible in the sources.
# EVERY CALLER MUST WRAP THIS IN @(): PowerShell unrolls a single result to a bare string, which
# then splats one CHARACTER at a time into the compiler's argument list.
function Get-Sources($dir, [switch]$Recurse) {
    $files = if ($Recurse) { Get-ChildItem $dir -Filter *.cs -Recurse } else { Get-ChildItem $dir -Filter *.cs }
    $full = @($files | ForEach-Object { $_.FullName })
    if (-not $full.Count) { return @() }
    $prefix = $dir.TrimEnd('\', '/').Length + 1
    $keys = @($full | ForEach-Object { $_.Substring($prefix).Replace('\', '/') })
    $paths = @($full)
    [Array]::Sort($keys, $paths, [System.StringComparer]::Ordinal)
    $paths
}

# The connector STANDARDS a board offers a socket of, deduplicated. A board with a socket can carry
# an extension board, and a program written for that pairing needs the EXTENSION's descriptors as
# well as the board's -- so the assembly for a board with sockets includes the emissions of every
# extension built to a standard it offers.
#
# WHICH EXTENSION IS ACTUALLY PLUGGED IN IS NOT A FACT EITHER FILE HOLDS, and that is deliberate:
# a board's own emission says what is plugged into a socket is not board truth, and an extension
# does not know its host. So the assembly carries the descriptors of everything that COULD fit and
# names nothing as fitted; a program picks one and states its own assumption.
function Resolve-ConnectorStandards($boardDir) {
    $toml = Join-Path $boardDir 'board.toml'
    if (-not (Test-Path $toml)) { return @() }
    $inConnector = $false
    $standards = New-Object System.Collections.Generic.List[string]
    foreach ($line in [System.IO.File]::ReadLines($toml)) {
        $trimmed = $line.Trim()
        if ($trimmed -eq '[[connectors]]') { $inConnector = $true; continue }
        # Any other section header ends the row -- including a `[[connectors.<name>.pins]]`, whose
        # rows state a position and never a standard.
        if ($trimmed.StartsWith('[')) { $inConnector = $false; continue }
        if (-not $inConnector) { continue }
        if ($trimmed -match '^standard\s*=\s*"([^"]+)"') {
            if (-not $standards.Contains($Matches[1])) { $standards.Add($Matches[1]) }
        }
    }
    $standards
}

# The extension emissions built to any of those standards. Sorted by path for the same reason
# Get-Sources sorts: metadata layout must not depend on enumeration order.
function Resolve-ExtensionSources($root, $standards) {
    if (-not $standards -or -not $standards.Count) { return @() }
    $extRoot = Join-Path $root 'ext'
    if (-not (Test-Path $extRoot)) { return @() }
    $out = New-Object System.Collections.Generic.List[string]
    foreach ($dir in @(Get-ChildItem $extRoot -Directory | Sort-Object Name)) {
        $toml = Join-Path $dir.FullName 'extension.toml'
        if (-not (Test-Path $toml)) { continue }
        $standard = ''
        # The header runs to the first ARRAY section, which is the strata reader's own rule: an
        # extension states its standard inside `[table]`, so stopping at the first `[` would stop
        # at `[table]` itself and find nothing. That was this function's first bug.
        foreach ($line in [System.IO.File]::ReadLines($toml)) {
            $trimmed = $line.Trim()
            if ($trimmed.StartsWith('[[')) { break }
            if ($trimmed -match '^standard\s*=\s*"([^"]+)"') { $standard = $Matches[1]; break }
        }
        if (-not $standards.Contains($standard)) { continue }
        $csharp = Join-Path $dir.FullName 'csharp'
        if (Test-Path $csharp) { foreach ($f in @(Get-Sources $csharp -Recurse)) { $out.Add($f) } }
    }
    $out
}

# The chip family whose C# a board needs. A board states `family` directly; a MODULE board states
# `module`, and the module states the family it wraps (the ATSAMW25 is a SAMD21G18A plus a radio,
# so its boards compile the samd21 drivers).
function Resolve-Family($boardDir) {
    $toml = Get-Content (Join-Path $boardDir 'board.toml') -Raw
    if ($toml -match '(?m)^\s*family\s*=\s*"([^"]+)"') { return $Matches[1] }
    if ($toml -match '(?m)^\s*module\s*=\s*"([^"]+)"') {
        $modulePath = Join-Path $root "csp/$($Matches[1])/module.toml"
        if (-not (Test-Path $modulePath)) { throw "$($boardDir): module '$($Matches[1])' has no csp/*/module.toml" }
        if ((Get-Content $modulePath -Raw) -match '(?m)^\s*family\s*=\s*"([^"]+)"') { return $Matches[1] }
        throw "$($boardDir): module '$($Matches[1])' states no family"
    }
    throw "$($boardDir)/board.toml states neither family nor module"
}

# The assembly's board name, taken from the GENERATED bindings file rather than re-derived from the
# directory name. The generator already decided how `feather-m0-adalogger` becomes
# `FeatherM0Adalogger`; re-implementing that rule here is a second source that can disagree with the
# first, and the class the board file declares is the one that has to match.
function Resolve-BoardName($csharpDir, $dirName) {
    $bindings = @(Get-ChildItem $csharpDir -Filter '*Bindings.g.cs')
    if ($bindings.Count -ne 1) {
        throw "bsp/$dirName/csharp: expected exactly one *Bindings.g.cs to name the board, found $($bindings.Count). Regenerate the family."
    }
    $bindings[0].Name -replace 'Bindings\.g\.cs$', ''
}

# The assembly's VENDOR segment, read from the same generated file for the same reason: the
# generator already decided that `raspberry-pi` becomes `RaspberryPi`, and it emits the decided
# value rather than the kebab one precisely so nothing downstream re-applies that rule.
function Resolve-BoardVendor($csharpDir, $dirName) {
    $bindings = @(Get-ChildItem $csharpDir -Filter '*Bindings.g.cs')
    $text = Get-Content $bindings[0].FullName -Raw
    if ($text -notmatch 'BOARD_VENDOR\s*=\s*"([^"]+)"') {
        throw "bsp/$dirName/csharp/$($bindings[0].Name): no BOARD_VENDOR. Regenerate the family."
    }
    $Matches[1]
}

# --- Build --------------------------------------------------------------------------------------
$boards = @(Get-ChildItem (Join-Path $root 'bsp') -Directory |
    Where-Object { Test-Path (Join-Path $_.FullName 'csharp') } |
    Sort-Object Name)
if ($Board) {
    $boards = @($boards | Where-Object { $_.Name -eq $Board })
    if (-not $boards.Count) { throw "no board '$Board' with a csharp/ directory under bsp/" }
}
if (-not $boards.Count) { throw 'no bsp/*/csharp directories found' }

$built = @()
foreach ($boardDir in $boards) {
    $name = Resolve-BoardName (Join-Path $boardDir.FullName 'csharp') $boardDir.Name
    $vendor = Resolve-BoardVendor (Join-Path $boardDir.FullName 'csharp') $boardDir.Name
    $qualified = "$vendor.$name"
    $family = Resolve-Family $boardDir.FullName
    $familyCs = Join-Path $root "csp/$family/csharp"
    # A board whose family has no C# cannot be built, and that is a failure rather than a skip:
    # skipping would leave the board with no assembly and no error, which surfaces much later as a
    # program unable to reference its own board.
    if (-not (Test-Path $familyCs)) {
        throw "bsp/$($boardDir.Name) needs csp/$family/csharp, which does not exist. Generate the family's C# or the board class cannot compile."
    }

    $standards = @(Resolve-ConnectorStandards $boardDir.FullName)
    $extSrc = @(Resolve-ExtensionSources $root $standards)
    $src = @(Get-Sources $familyCs -Recurse) + @(Get-Sources (Join-Path $boardDir.FullName 'csharp') -Recurse) + $extSrc
    $dll = Join-Path $out "Lamella.Boards.$qualified.dll"
    $refs = @($BoardReferences | ForEach-Object { "/reference:$(Join-Path $out "$_.dll")" })

    $extNote = if ($extSrc.Count) { " + $($extSrc.Count) ext" } else { '' }
    Write-Host "Lamella.Boards.$qualified ($($src.Count) sources: csp/$family + bsp/$($boardDir.Name)$extNote) -> $dll"
    & $Lcsc @src @defineArg @refs /target:library "/out:$dll" /debug-
    if ($LASTEXITCODE -ne 0) { throw "Lamella.Boards.$qualified compile failed ($LASTEXITCODE)" }
    $built += [pscustomobject]@{ Board = $boardDir.Name; Assembly = "Lamella.Boards.$qualified"; Bytes = (Get-Item $dll).Length }
}

Write-Host ''
$built | ForEach-Object { '  {0,-34} {1,8:N0} bytes' -f $_.Assembly, $_.Bytes }
Write-Host ''
Write-Host "Done. $($built.Count) board assemblies in $out"
