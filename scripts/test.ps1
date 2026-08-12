# Local test runner for nd-organizer.
#
# Runs the host unit tests, a Clippy lint pass, and a wasm build check so the
# plugin (the wasm module) is exercised too - not just the pure logic.
#
# Usage:
#   pwsh ./scripts/test.ps1            # tests + clippy + wasm build
#   pwsh ./scripts/test.ps1 -SkipClippy
#   pwsh ./scripts/test.ps1 -SkipWasm

param(
    [switch]$SkipClippy,
    [switch]$SkipWasm
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

Write-Host "==> cargo test"
cargo test
if ($LASTEXITCODE -ne 0) { throw "tests failed" }

if (-not $SkipClippy) {
    Write-Host "==> cargo clippy"
    cargo clippy --all-targets
    if ($LASTEXITCODE -ne 0) { throw "clippy failed" }
}

if (-not $SkipWasm) {
    Write-Host "==> wasm build check"
    rustup target list --installed | Select-String -Quiet "wasm32-wasip1" | Out-Null
    if ($LASTEXITCODE -ne 0) { rustup target add wasm32-wasip1 }
    cargo build --release --target wasm32-wasip1
    if ($LASTEXITCODE -ne 0) { throw "wasm build failed" }
    Write-Host "==> plugin.wasm built:"
    Get-Item "target\wasm32-wasip1\release\nd_organizer.wasm" | Select-Object Name, Length
}

Write-Host "All checks passed."
