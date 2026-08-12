# Builds plugin.wasm and packages nd-organizer.ndp
#
# Usage:
#   pwsh ./scripts/build.ps1          # build wasm + package .ndp into ./dist
#   pwsh ./scripts/build.ps1 -Install # also copy .ndp to the Navidrome plugins share
#
# Requires: Rust (rustup + wasm32-wasip1 target). Zig is needed from Phase 2
# (chromaprint cross-compile) but not for this script today.

param(
    [switch]$Install
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

cargo build --release --target wasm32-wasip1
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$dist = Join-Path $root "dist"
New-Item -ItemType Directory -Force -Path $dist | Out-Null

$wasm = Join-Path $root "target\wasm32-wasip1\release\nd_organizer.wasm"
$stage = Join-Path $dist ".stage"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Copy-Item (Join-Path $root "manifest.json") $stage
Copy-Item $wasm (Join-Path $stage "plugin.wasm")

$ndp = Join-Path $dist "nd-organizer.ndp"
if (Test-Path $ndp) { Remove-Item $ndp -Force }

# .ndp is a plain zip with manifest.json + plugin.wasm at the root.
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $ndp -Force
Remove-Item $stage -Recurse -Force

Write-Host "Built $ndp"

if ($Install) {
    $share = "\\192.168.0.21\opt\navidrome\data\plugins"
    if (Test-Path $share) {
        Copy-Item $ndp $share -Force
        Write-Host "Installed to $share"
        Write-Host "Next: rescan plugins in the Navidrome UI, then grant write access:"
        Write-Host "  navidrome plugin edit nd-organizer --write-access --all-libraries"
    } else {
        Write-Warning "Share not reachable: $share"
    }
}
