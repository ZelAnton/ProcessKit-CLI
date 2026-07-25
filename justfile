# Local dev-lane entry points that mirror `.github/workflows/{ci,fuzz,mutants}.yml`
# job-for-job, so `just <target>` never drifts from what CI actually runs. See
# CONTRIBUTING.md for the narrative docs on each tier; this file is the
# executable index. Install `just`: https://github.com/casey/just#installation
# (dev-only tooling — CI itself invokes each command directly, not through
# this file).

# List available recipes (what bare `just` runs).
default:
    @just --list

# One-time machine-readiness check (Rust toolchain on PATH) — see
# scripts/check-env.sh / scripts/check-env.ps1. Run this once on a fresh
# clone; the recipes below assume it already passed and do not re-check.
[unix]
check-env:
    bash scripts/check-env.sh

[windows]
check-env:
    pwsh -File scripts/check-env.ps1

# Default test tier — mirrors ci.yml's `test` job (build --all-targets, then
# `cargo test`). Excludes the feature-gated `e2e` tier below.
test:
    cargo build --all-targets
    cargo test

# Opt-in end-to-end containment tier — mirrors ci.yml's `e2e` job.
e2e:
    cargo test --features e2e --test e2e -- --nocapture

# Clippy with CI's exact flags — mirrors ci.yml's `clippy` job. `--all-features`
# so the `e2e`-gated tier and its helper worker are linted too.
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Formatting check (no changes written) — mirrors ci.yml's `fmt` job.
fmt-check:
    cargo fmt --all --check

# Apply formatting locally. Not a CI job itself (fmt-check above is what CI
# runs) — the write-mode counterpart for local use before committing.
fmt:
    cargo fmt --all

# Dependency/license/advisory audit — mirrors ci.yml's `audit` job
# (EmbarkStudios/cargo-deny-action, `command: check advisories bans licenses
# sources`). See K-017: deny.toml now gates all four categories, so a bare
# `cargo deny check` is equivalent — this spells out the same categories CI
# passes explicitly so the equivalence never has to be re-derived from memory.
deny:
    cargo deny check advisories bans licenses sources

# Coverage report — mirrors the scope of ci.yml's non-gating `coverage` job
# (the default `cargo test` tier, not the feature-gated `e2e` tier). `--open`
# builds an HTML report and opens it; CI instead reuses the same instrumented
# run's profile for a step-summary table via `--no-report` + `report` (see
# .github/workflows/ci.yml) — informational either way, drop `--open` for a
# plain terminal summary.
coverage:
    cargo llvm-cov --open

# Criterion benchmarks — mirrors ci.yml's non-gating `perf` job.
bench:
    cargo bench --features bench

# Short, bounded cargo-fuzz smoke run — NOT fuzz.yml's full scheduled/dispatch
# budget (120s default, more on manual dispatch). Default target and a short
# per-target budget, both overridable:
#   just fuzz-smoke                  # registry_record, 10s
#   just fuzz-smoke control_wire 30  # a different target/budget
# Requires the nightly toolchain and cargo-fuzz — see CONTRIBUTING.md,
# "Fuzzing".
fuzz-smoke target='registry_record' seconds='10':
    cd fuzz && cargo +nightly fuzz run {{ target }} -- -max_total_time={{ seconds }}

# cargo-mutants — mirrors mutants.yml's invocation shape (`cargo mutants
# [args]`). Config is auto-discovered from .cargo/mutants.toml (see K-054 —
# it is NOT picked up from a repo-root mutants.toml), so this recipe never
# passes --config and relies on that auto-discovery. A bare `just mutants`
# runs the full scoped tree (multi-hour — see CONTRIBUTING.md, "Mutation
# testing"); prefer scoping it yourself, mirroring how CI shards the work:
#   just mutants --file src/hash.rs
#   just mutants --shard 0/8
mutants *args='':
    cargo mutants {{ args }}

# Spelling + internal-link/anchor checks — mirrors ci.yml's gating `typos` and
# `docs-links` jobs (the `--offline`, internal-only scope). The separate
# non-gating `docs-links-external` job additionally checks live http(s) URLs
# over a real network connection and is deliberately not mirrored here, same
# reason it does not gate CI: it can flake for reasons unrelated to this repo.
docs-checks:
    typos .
    lychee --offline --config .lychee.toml README.md CONTRIBUTING.md SECURITY.md CHANGELOG.md 'docs/**/*.md'
