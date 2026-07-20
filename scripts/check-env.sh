#!/usr/bin/env bash
#
# Checks this machine can build and test this Rust crate before you initialize
# the template (POSIX counterpart of check-env.ps1 — use whichever matches your
# shell; both do the same thing).
#
# Verifies the Rust toolchain (cargo + rustc) is on PATH. rust-toolchain.toml
# pins the channel and components (rustfmt, clippy), which rustup installs
# automatically on the first build, so only cargo/rustc need to be present.
# Exits 0 when ready; if the toolchain is missing it prints per-OS install
# commands and exits 1 — install it, then re-run.
#
# Usage: bash ./scripts/check-env.sh

set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,13p' "$0"; exit 0 ;; esac

problems=()
echo "==> Checking environment for Rust development"

# Required: cargo (build/test driver) and rustc (the compiler).
command -v cargo >/dev/null 2>&1 || problems+=("cargo ('cargo' is not on PATH)")
command -v rustc >/dev/null 2>&1 || problems+=("the Rust compiler ('rustc' is not on PATH)")
if [ ${#problems[@]} -eq 0 ]; then
  echo "    $(rustc --version)"
fi

if [ ${#problems[@]} -eq 0 ]; then
  echo
  echo "Environment ready. Next: bash ./scripts/init.sh --project-name ..."
  echo "(rustup installs the pinned stable + rustfmt/clippy on the first cargo build.)"
  exit 0
fi

echo
echo "Environment NOT ready. Missing:"
for p in "${problems[@]}"; do echo "  - $p"; done
echo
echo "Install the Rust toolchain via rustup, then re-run this check:"
echo "  Windows : winget install Rustlang.Rustup ; rustup default stable"
echo "  macOS   : brew install rustup ; rustup-init"
echo "  Linux   : curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
echo "  (any OS) : see https://rustup.rs"
exit 1
