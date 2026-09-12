# make-releases.ps1 —— assemble per-target release folders under target\releases\
#
# Called by scripts\build-all.ps1 after all 6 targets have been cross-built.
# For each target it lays out three publishable packages:
#   PointerSes-<ver>-<os>_<arch>-all   four tools + LLVM-C.dll (llvm backend runtime)
#   PointerSes-<ver>-<os>_<arch>-bin   four tools only
#   PointerSes-<ver>-<os>_<arch>-dev   tools + LLVM-C.dll + cdylib + rlib (embedding)
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

Write-Host ("releases assembled under {0} ({1} target(s) x all/bin/dev)" -f $out, $built)
