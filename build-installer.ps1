<#
.SYNOPSIS
    Build blip's release binaries and wrap them in a per-user installer.

.DESCRIPTION
    Stops any running daemon first — a live blipd.exe holds a file lock and the
    linker cannot overwrite it, which surfaces as a bare "os error 5 (拒绝访问)"
    that gives no hint about the real cause.

    Output lands in dist\blip-<version>-setup.exe.
#>
[CmdletBinding()]
param(
    # Skip cargo and package whatever is already in target\release.
    [switch]$NoBuild
)

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

function Find-ISCC {
    $c = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($c) { return $c.Source }
    # scoop shims aren't always on PATH in a non-interactive shell
    $candidates = @(
        "$env:USERPROFILE\scoop\apps\inno-setup\current\ISCC.exe"
        "$env:SCOOP\apps\inno-setup\current\ISCC.exe"
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    )
    foreach ($p in $candidates) { if ($p -and (Test-Path $p)) { return $p } }
    throw "ISCC.exe not found. Install it with:  scoop install extras/inno-setup"
}

Write-Host '==> stopping any running daemon' -ForegroundColor Cyan
$cli = Join-Path $PSScriptRoot 'target\release\blip.exe'
if (Test-Path $cli) { & $cli --quit 2>$null }
Start-Sleep -Milliseconds 400
# Routed through cmd deliberately: with $ErrorActionPreference = 'Stop', a
# native command writing to stderr becomes a terminating error, and taskkill
# always does that when the process is already gone — which is the normal case.
cmd /c 'taskkill /F /IM blipd.exe >nul 2>&1'

if (-not $NoBuild) {
    Write-Host '==> cargo build --release' -ForegroundColor Cyan
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }

    Write-Host '==> cargo test' -ForegroundColor Cyan
    cargo test --quiet
    if ($LASTEXITCODE -ne 0) { throw "tests failed ($LASTEXITCODE)" }
}

# Single source of truth for the version: Cargo.toml. Passed to ISCC rather
# than duplicated in the .iss, so the two can never drift.
$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
Write-Host "==> packaging blip $version" -ForegroundColor Cyan

$iscc = Find-ISCC
New-Item -ItemType Directory -Force -Path dist | Out-Null
& $iscc "/DAppVersion=$version" installer\blip.iss
if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }

$out = "dist\blip-$version-setup.exe"
$size = [math]::Round((Get-Item $out).Length / 1MB, 2)
Write-Host "==> $out  ($size MB)" -ForegroundColor Green
