# make-releases.ps1 -- stage per-target release folders, then package into zips.
#
# Called by scripts\build-all.ps1 after all 6 targets have been cross-built.
#
# Phase 1 (staging): lay out, under target\releases\, three publishable packages
# per target:
#   PointerSes-<ver>-<os>_<arch>-all   four tools + LLVM-C.dll (llvm backend runtime)
#   PointerSes-<ver>-<os>_<arch>-bin   four tools only
#   PointerSes-<ver>-<os>_<arch>-dev   tools + LLVM-C.dll + cdylib + rlib (embedding)
# plus shared docs/ and examples/ folders.
#
# Phase 2 (packaging): under <root>\releases\ (created if missing) produce:
#   pointerses-<ver>-allreleases.zip         everything in target\releases at the zip root
#   pointerses-<ver>-<os>_<arch>-<level>.zip one per staged target folder: that folder
#                                          (kept intact) + docs/ + examples/ at the zip root
#
#   pwsh -File scripts\make-releases.ps1        (or powershell.exe)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$out  = Join-Path $root 'target\releases'
$version = '0.1-beta'

$targets = @(
    @{ triple = 'x86_64-unknown-linux-gnu';      os = 'linux'; arch = 'x64' },
    @{ triple = 'i686-unknown-linux-gnu';        os = 'linux'; arch = 'x32' },
    @{ triple = 'aarch64-unknown-linux-gnu';     os = 'linux'; arch = 'arm64' },
    @{ triple = 'x86_64-pc-windows-gnu';         os = 'win';   arch = 'x64' },
    @{ triple = 'i686-pc-windows-gnu';           os = 'win';   arch = 'x32' },
    @{ triple = 'aarch64-pc-windows-gnullvm';    os = 'win';   arch = 'arm64' }
)

# ===========================================================================
# Phase 1 - stage the per-target folders under target\releases\
# ===========================================================================
$built = 0
foreach ($t in $targets) {
    $rel = Join-Path $root ("target\{0}\release" -f $t.triple)
    if (-not (Test-Path $rel)) {
        Write-Host ("WARNING: {0} has no release output; skipping" -f $t.triple)
        continue
    }

    $exe = ''
    if ($t.os -eq 'win') { $exe = '.exe' }
    $base = "PointerSes-$version-{0}_{1}" -f $t.os, $t.arch

    # the four tools
    $tools = @()
    foreach ($tool in @('pss', 'pssc', 'pssp', 'pssl')) {
        $tools += Join-Path $rel ("{0}{1}" -f $tool, $exe)
    }

    # --- -all: tools + LLVM-C.dll (llvm backend runtime) ---
    $dAll = Join-Path $out "$base-all"
    New-Item -ItemType Directory -Force -Path $dAll | Out-Null
    $tools | ForEach-Object { Copy-Item $_ $dAll -Force }
    $llvm = Join-Path $rel 'LLVM-C.dll'
    # LLVM-C.dll is a Windows-only runtime (bundled by build.rs into each target's
    # release dir even when cross-compiling); Linux resolves libLLVM-C.so from the
    # system at runtime, so only ship the DLL in Windows packages.
    if ($t.os -eq 'win' -and (Test-Path $llvm)) { Copy-Item $llvm $dAll -Force }

    # --- -bin: tools only ---
    $dBin = Join-Path $out "$base-bin"
    New-Item -ItemType Directory -Force -Path $dBin | Out-Null
    $tools | ForEach-Object { Copy-Item $_ $dBin -Force }

    # --- -dev: tools + LLVM-C.dll + cdylib + rlib + import lib ---
    $dDev = Join-Path $out "$base-dev"
    New-Item -ItemType Directory -Force -Path $dDev | Out-Null
    $tools | ForEach-Object { Copy-Item $_ $dDev -Force }
    if ($t.os -eq 'win' -and (Test-Path $llvm)) { Copy-Item $llvm $dDev -Force }

    # cdylib: pointerses.dll (win) / libpointerses.so (linux)
    $cdylib = if ($t.os -eq 'win') { 'pointerses.dll' } else { 'libpointerses.so' }
    $cdylibSrc = Join-Path $rel $cdylib
    if (Test-Path $cdylibSrc) { Copy-Item $cdylibSrc $dDev -Force }

    # mingw import library / rlib
    foreach ($dev in @('libpointerses.dll.a', 'libpointerses.rlib')) {
        $p = Join-Path $rel $dev
        if (Test-Path $p) { Copy-Item $p $dDev -Force }
    }

    $built++
    Write-Host ("  {0}  <- {1}" -f $base, $t.triple)
}

# --- docs, license & examples (shared, staged once under target\releases\) ---
#   project/examples/*  -> target/releases/examples/*  (nested cargo target/ stripped)
#   project/README.md   -> target/releases/docs/README.md
#   project/docs.md     -> target/releases/docs/docs.md
#   project/LICENSE     -> target/releases/docs/LICENSE
# Note: destination folders are removed first so re-running this script cannot
# nest an existing target dir inside itself (Copy-Item into an existing dir).
$dExamples = Join-Path $out 'examples'
if (Test-Path $dExamples) { Remove-Item $dExamples -Recurse -Force }
New-Item -ItemType Directory -Force -Path $dExamples | Out-Null
$examplesSrc = Join-Path $root 'examples'
if (Test-Path $examplesSrc) {
    Get-ChildItem $examplesSrc -Force | ForEach-Object {
        Copy-Item $_.FullName (Join-Path $dExamples $_.Name) -Recurse -Force
    }
    # examples\native_rust is a separate cargo crate; if someone built it in
    # place, its target/ would be copied straight into the package. Strip any
    # nested cargo output so only source ships (deepest first, so removal
    # cannot fail on a still-non-empty parent).
    Get-ChildItem $dExamples -Recurse -Directory -Filter 'target' -ErrorAction SilentlyContinue |
        Sort-Object { $_.FullName.Length } -Descending |
        ForEach-Object { Remove-Item $_.FullName -Recurse -Force }
    Write-Host ("  examples/ staged under {0}" -f $dExamples)
}
$dDocs = Join-Path $out 'docs'
if (Test-Path $dDocs) { Remove-Item $dDocs -Recurse -Force }
New-Item -ItemType Directory -Force -Path $dDocs | Out-Null
foreach ($doc in @('README.md', 'docs.md', 'LICENSE')) {
    $p = Join-Path $root $doc
    if (Test-Path $p) { Copy-Item $p (Join-Path $dDocs $doc) -Force }
}
Write-Host ("  docs/ staged under {0}" -f $dDocs)

Write-Host ("staged under {0} ({1} target(s) x all/bin/dev)" -f $out, $built)

# ===========================================================================
# Phase 2 - package the staged tree into zips under <root>\releases\
# ===========================================================================
$zipsOut = Join-Path $root 'releases'
if (-not (Test-Path $zipsOut)) { New-Item -ItemType Directory -Force -Path $zipsOut | Out-Null }
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

# Add every file under $srcDir to $archive at "<prefix>/<rel>", where <rel> is
# the path relative to $srcDir. Files are referenced in place (no copy), so the
# 67 MB LLVM-C.dll in the Windows -all/-dev folders is not duplicated to disk.
function Add-DirToArchive($archive, $srcDir, $prefix) {
    if (-not (Test-Path $srcDir)) { return }
    $base = (Get-Item $srcDir).FullName.TrimEnd('\')
    Get-ChildItem $base -Recurse -File -Force -ErrorAction SilentlyContinue | ForEach-Object {
        $rel = $_.FullName.Substring($base.Length + 1).Replace('\', '/')
        [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $_.FullName, "$prefix/$rel") | Out-Null
    }
}

# (1) one bundle with everything (contents of target\releases at the zip root).
#     Built with the same manual entry walk as the per-target zips so entry
#     paths use '/' (portable); .NET Framework's CreateFromDirectory would emit
#     '\' separators, which break extraction on Linux.
$allZip = Join-Path $zipsOut ("pointerses-{0}-allreleases.zip" -f $version)
if (Test-Path $allZip) { Remove-Item $allZip -Force }
$archive = [System.IO.Compression.ZipFile]::Open($allZip, [System.IO.Compression.ZipArchiveMode]::Create)
try {
    Get-ChildItem $out -Directory | ForEach-Object { Add-DirToArchive $archive $_.FullName $_.Name }
} finally {
    $archive.Dispose()
}
Write-Host ("  {0}" -f (Split-Path $allZip -Leaf))

# (2) one zip per staged target folder: the folder kept intact + docs + examples
#     at the zip root. Zip name = folder name lowercased + .zip
#     (PointerSes-0.1-beta-linux_arm64-dev -> pointerses-0.1-beta-linux_arm64-dev.zip).
$docsSrc  = Join-Path $out 'docs'
$exampSrc = Join-Path $out 'examples'
$perTarget = 0
Get-ChildItem $out -Directory | Where-Object { $_.Name -ne 'docs' -and $_.Name -ne 'examples' } | ForEach-Object {
    $folder  = $_.Name
    $zipName = $folder.ToLower() + '.zip'
    $zipPath = Join-Path $zipsOut $zipName
    if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
    $archive = [System.IO.Compression.ZipFile]::Open($zipPath, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        Add-DirToArchive $archive $_.FullName $folder
        Add-DirToArchive $archive $docsSrc  'docs'
        Add-DirToArchive $archive $exampSrc 'examples'
    } finally {
        $archive.Dispose()
    }
    $perTarget++
    Write-Host ("  {0}" -f $zipName)
}

Write-Host ("release zips: 1 allreleases + {0} per-target, under {1}" -f $perTarget, $zipsOut)
