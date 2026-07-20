#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Checks this machine can build and test this Rust crate before you initialize
    the template.

.DESCRIPTION
    Verifies the Rust toolchain (cargo + rustc) is on PATH. rust-toolchain.toml
    pins the channel and components (rustfmt, clippy), which rustup installs
    automatically on the first build, so only cargo/rustc need to be present.
    Prints "Environment ready" and exits 0 on success; if the toolchain is missing
    it prints per-OS install commands and exits 1 — install it, then re-run.

    Run it first, before scripts/init.ps1:

        pwsh ./scripts/check-env.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$problems = @()

Write-Host "==> Checking environment for Rust development" -ForegroundColor Cyan

# Required: cargo (build/test driver) and rustc (the compiler).
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $problems += "cargo ('cargo' is not on PATH)"
}
if (-not (Get-Command rustc -ErrorAction SilentlyContinue)) {
    $problems += "the Rust compiler ('rustc' is not on PATH)"
}
if ($problems.Count -eq 0) {
    $ver = (& rustc --version) 2>$null
    Write-Host "    $ver" -ForegroundColor DarkGray
}

if ($problems.Count -eq 0) {
    Write-Host ""
    Write-Host "Environment ready. Next: pwsh ./scripts/init.ps1 -ProjectName ..." -ForegroundColor Green
    Write-Host "(rustup installs the pinned stable + rustfmt/clippy on the first cargo build.)" -ForegroundColor DarkGray
    exit 0
}

Write-Host ""
Write-Host "Environment NOT ready. Missing:" -ForegroundColor Red
foreach ($p in $problems) { Write-Host "  - $p" -ForegroundColor Red }
Write-Host ""
Write-Host "Install the Rust toolchain via rustup, then re-run this check:" -ForegroundColor Yellow
Write-Host "  Windows : winget install Rustlang.Rustup ; rustup default stable"
Write-Host "  macOS   : brew install rustup ; rustup-init"
Write-Host "  Linux   : curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
Write-Host "  (any OS) : see https://rustup.rs"
exit 1
