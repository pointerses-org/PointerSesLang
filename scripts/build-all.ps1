# One-shot: cross-build every target, then assemble the release folders.
#
# This is the single entry point to produce the distributables:
#   0. recompile the zigcc shim (native, for the host) — it is gitignored
#      and derived from tools\zigcc.rs, and it is the linker for every cross
#      target below, so it must exist before step 1, not after it
#   1. cargo build --release --target <triple> for the four tools
#   2. cargo build --release --target <triple> for the cdylib / rlib
#   3. invoke scripts\make-releases.ps1 to lay out target\releases\
#   4. verify the toolchain by packaging examples\main.psp
#
# Requirements: rustup targets installed (see README), zig on PATH or ZIG=,
# and the zigcc shim buildable from tools\zigcc.rs (rustc on PATH).
#
#   pwsh -File scripts\build-all.ps1

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
$out  = Join-Path $root 'target\releases'

# --- locate zig (PATH first, then ZIG env) ---
$zigDir = $null
$zigCmd = Get-Command zig -ErrorAction SilentlyContinue
if ($zigCmd) { $zigDir = Split-Path $zigCmd.Source -Parent }
if (-not $zigDir -and $env:ZIG) { $zigDir = Split-Path $env:ZIG -Parent }
if ($zigDir -and (Test-Path (Join-Path $zigDir 'zig.exe'))) {
    $env:PATH = "$zigDir;$env:PATH"
    Write-Host "zig: $zigDir"
} else {
    Write-Host "WARNING: zig not found on PATH/ZIG — tools will build, but pssc runtime cross-linking needs zig."
}
if (Test-Path "$root\tools") { $env:PATH = "$root\tools;$env:PATH" }

$targets = @(
    @{ triple = 'x86_64-unknown-linux-gnu';      os = 'linux';    arch = 'x64' },
    @{ triple = 'i686-unknown-linux-gnu';        os = 'linux';    arch = 'x32' },
    @{ triple = 'aarch64-unknown-linux-gnu';     os = 'linux';    arch = 'arm64' },
    @{ triple = 'x86_64-pc-windows-gnu';         os = 'win';      arch = 'x64' },
    @{ triple = 'i686-pc-windows-gnu';           os = 'win';      arch = 'x32' },
    @{ triple = 'aarch64-pc-windows-gnullvm';    os = 'win';      arch = 'arm64' }
)

$failed = @()

# --- 0. zigcc shim: the linker for every cross target below ---
Write-Host "===== zigcc shim ====="
rustc --edition 2021 -O (Join-Path $root 'tools\zigcc.rs') -o (Join-Path $root 'tools\zigcc.exe') 2>&1 | ForEach-Object { "$_" } | Out-String -Width 200
if (-not (Test-Path (Join-Path $root 'tools\zigcc.exe'))) {
    # tools\zigcc.exe is gitignored and derived from tools\zigcc.rs, so a fresh
    # clone has none. Without it every cross build fails with
    # `linker 'zigcc' not found`, so fail here instead of six times below.
    Write-Host "zigcc shim build FAILED — it is the linker for every target below"
    Write-Host "need rustc on PATH to build it from tools\zigcc.rs"
    exit 1
}
Write-Host "zigcc.exe rebuilt"

# --- 1. build tools for every target ---
Push-Location $root
foreach ($t in $targets) {
    Write-Host "===== BUILDING $($t.triple) (tools) ====="
    cargo build --release --target $t.triple --bin pss --bin pssc --bin pssp --bin pssl 2>&1 | ForEach-Object { "$_" } | Out-String -Width 200
    if ($LASTEXITCODE -eq 0) { Write-Host "$($t.triple) tools OK" }
    else { Write-Host "$($t.triple) tools FAILED"; $failed += "$($t.triple) tools" }
}
# --- 2. build the cdylib (pointerses.dll/.so + rlib) for every target ---
foreach ($t in $targets) {
    Write-Host "===== BUILDING $($t.triple) (cdylib) ====="
    cargo build --release --target $t.triple 2>&1 | ForEach-Object { "$_" } | Out-String -Width 200
    if ($LASTEXITCODE -eq 0) { Write-Host "$($t.triple) cdylib OK" }
    else { Write-Host "$($t.triple) cdylib FAILED"; $failed += "$($t.triple) cdylib" }
}
Pop-Location

# --- 3. assemble the release folders ---
if (Test-Path (Join-Path $root 'scripts\make-releases.ps1')) {
    Write-Host "===== ASSEMBLING releases ====="
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $root 'scripts\make-releases.ps1')
} else {
    Write-Host "WARNING: scripts\make-releases.ps1 missing; skip assembly"
    $failed += "make-releases"
}

# --- 4. verify the toolchain + language features by packaging the
#        comprehensive example (host native, then multi-arch if zig is present) ---
$psscHost = Get-Command pssc -ErrorAction SilentlyContinue
if (-not $psscHost) {
    $cand = Join-Path $root 'target\debug\pssc.exe'
    if (Test-Path $cand) { $psscHost = $cand }
}
$demoSrc = Join-Path $root 'examples\main.psp'
if ($psscHost -and (Test-Path $demoSrc)) {
    Write-Host "===== PACKAGING examples\main.psp (host native) ====="
    & $psscHost $demoSrc --file-exe -o (Join-Path $root 'target\demo.exe') 2>&1 | ForEach-Object { "$_" } | Out-String -Width 200
    if ($LASTEXITCODE -eq 0) { Write-Host "demo.exe packaged OK" }
    else { Write-Host "demo.exe packaging FAILED"; $failed += "demo packaging" }

    if ($zigDir -or (Get-Command zig -ErrorAction SilentlyContinue)) {
        Write-Host "===== PACKAGING examples\main.psp (multi-arch x64/x32/arm64) ====="
        & $psscHost $demoSrc --file-exe --fram-all -o (Join-Path $root 'target\demo') 2>&1 | ForEach-Object { "$_" } | Out-String -Width 200
        if ($LASTEXITCODE -eq 0) { Write-Host "multi-arch demo packaged OK" }
        else { Write-Host "multi-arch demo packaging FAILED"; $failed += "demo multi-arch" }
    }
} else {
    Write-Host "WARNING: pssc or examples\main.psp not found; skip example packaging"
}

# --- summary ---
Write-Host "`n===== BUILD-ALL SUMMARY ====="
if ($failed.Count) {
    Write-Host ("FAILED: " + ($failed -join '; '))
} else {
    Write-Host "ALL 6 TARGETS BUILT + RELEASES ASSEMBLED OK"
    if (Test-Path $out) {
        $folders = (Get-ChildItem $out -Directory).Count
        Write-Host "release folders under target\releases: $folders"
    }
}
Write-Host "ALL_DONE"