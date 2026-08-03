# Local dev-lane entry points covering the main dev-lanes (test, e2e, lint,
# fmt-check, deny, coverage, bench, fuzz-smoke, mutants, docs-checks). Each
# target below mirrors the exact command its corresponding CI job in
# `.github/workflows/{ci,fuzz,mutants}.yml` runs, so it never drifts from what
# CI actually does — but coverage is not complete: three gating ci.yml jobs
# have no target here (`yaml-lint`, `msrv`, `target-check`; see
# CONTRIBUTING.md, "Prerequisites", for why and how to run them directly), so
# a clean run of every target below does not by itself guarantee a green CI
# run. See CONTRIBUTING.md for the narrative docs on each tier; this file is
# the executable index. Install `just`:
# https://github.com/casey/just#installation (dev-only tooling — CI itself
# invokes each command directly, not through this file).

# List available recipes (what bare `just` runs).
default:
    @just --list

# One-time machine-readiness check (Rust toolchain on PATH) before any other
# recipe — see scripts/check-env.sh / scripts/check-env.ps1. Run this once on
# a fresh clone; the recipes below assume it already passed and do not
# re-check.

# Verify the Rust toolchain is on PATH (POSIX) — see scripts/check-env.sh.
[unix]
check-env:
    bash scripts/check-env.sh

# Verify the Rust toolchain is on PATH (Windows) — see scripts/check-env.ps1.
[windows]
check-env:
    pwsh -File scripts/check-env.ps1

# Excludes the feature-gated `e2e` tier below (see the `e2e` recipe).

# Default test tier — mirrors ci.yml's `test` job: `cargo build --all-targets`, then `cargo test`.
test:
    cargo build --all-targets
    cargo test

# Opt-in end-to-end containment tier — mirrors ci.yml's `e2e` job.
e2e:
    cargo test --features e2e --test e2e -- --nocapture

# `--all-features` so the `e2e`-gated tier and its helper worker are linted too.

# Clippy with CI's exact flags — mirrors ci.yml's `clippy` job.
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Formatting check (no changes written) — mirrors ci.yml's `fmt` job.
fmt-check:
    cargo fmt --all --check

# Not a CI job itself (fmt-check above is what CI runs).

# Apply formatting locally — the write-mode counterpart of fmt-check, for local use before committing.
fmt:
    cargo fmt --all

# EmbarkStudios/cargo-deny-action passes `command: check advisories bans
# licenses sources`. See K-017: deny.toml now gates all four categories, so a
# bare `cargo deny check` is equivalent — this spells out the same categories
# CI passes explicitly so the equivalence never has to be re-derived from
# memory.

# Dependency/license/advisory audit — mirrors ci.yml's `audit` job.
deny:
    cargo deny check advisories bans licenses sources

# Mirrors the scope of ci.yml's non-gating `coverage` job (the default
# `cargo test` tier, not the feature-gated `e2e` tier). CI instead reuses the
# same instrumented run's profile for a step-summary table via `--no-report` +
# `report` (see .github/workflows/ci.yml) — informational either way.

# Coverage report: builds and opens an HTML report (drop --open below for a plain terminal summary instead).
coverage:
    cargo llvm-cov --open

# Criterion benchmarks — mirrors ci.yml's non-gating `perf` job.
bench:
    cargo bench --features bench

# NOT fuzz.yml's full scheduled/dispatch budget (120s default, more on manual
# dispatch). Default target and a short per-target budget, both overridable:
#   just fuzz-smoke                        # registry_record, 10s
#   just fuzz-smoke control_wire 30        # a different target/budget
#   just fuzz-smoke runner_exit_tail 10    # wait --report-outcome's read-back (T-301)
# Default stays `registry_record` (a deliberate choice, not an oversight —
# there is no "primary" target among the four, so the original default is left
# undisturbed); every target fuzz.yml runs (see fuzz/fuzz_targets/*.rs) is a
# valid `target` argument here, `runner_exit_tail` included. Requires the
# nightly toolchain and cargo-fuzz — see CONTRIBUTING.md, "Fuzzing".

# Short, bounded cargo-fuzz smoke run for a quick local sanity check (not a real fuzzing session).
fuzz-smoke target='registry_record' seconds='10':
    cd fuzz && cargo +nightly fuzz run {{ target }} -- -max_total_time={{ seconds }}

# Config is auto-discovered from .cargo/mutants.toml (see K-054 — it is NOT
# picked up from a repo-root mutants.toml), so this recipe never passes
# --config and relies on that auto-discovery. A bare `just mutants` runs the
# full scoped tree (multi-hour — see CONTRIBUTING.md, "Mutation testing");
# prefer scoping it yourself, mirroring how CI shards the work:
#   just mutants --file src/hash.rs
#   just mutants --shard 0/8

# cargo-mutants — mirrors mutants.yml's invocation shape (`cargo mutants [args]`).
mutants *args='':
    cargo mutants {{ args }}

# The separate non-gating `docs-links-external` job additionally checks live
# http(s) URLs over a real network connection and is deliberately not
# mirrored here — that job flakes for reasons unrelated to this repo, so it
# does not gate CI (unlike the two jobs this recipe does mirror).

# Spelling + internal-link/anchor checks — mirrors ci.yml's gating `typos` and `docs-links` jobs.
docs-checks:
    typos .
    lychee --offline --config .lychee.toml README.md CONTRIBUTING.md SECURITY.md CHANGELOG.md 'docs/**/*.md' 'skills/**/*.md'

# Build the public mdBook site and verify every rendered local link and anchor.
docs:
    mdbook build
    python scripts/check_docs_links.py book
