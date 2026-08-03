# Contributing to processkit-cli

Thanks for your interest in improving **processkit-cli**.

Before making a non-trivial change, skim [the architecture
overview](docs/architecture.md) for how the modules fit together — the module
map, the data flow of one `run`, the control-plane contour, and the boundary
with the `processkit` crate.

## Prerequisites

- A stable Rust toolchain. The repo pins it via
  [`rust-toolchain.toml`](rust-toolchain.toml) (channel `stable`, with `rustfmt`
  and `clippy`), so `rustup` installs the right components automatically the
  first time you build.
- Your toolchain must be at least the project's **Minimum Supported Rust
  Version (MSRV)**, declared as `rust-version` in [`Cargo.toml`](Cargo.toml) and
  verified by the `msrv` CI job. `stable` is normally newer than the floor, so
  this only matters if you adopt a newer language or `std` feature — bump
  `rust-version` and the `msrv` job's toolchain together if you do.
- Optionally, [`just`](https://github.com/casey/just#installation) — the repo
  ships a [`justfile`](justfile) covering the main dev-lanes (`just test`,
  `just lint`, …); each target mirrors the exact command its CI counterpart
  runs, so it never drifts from what CI actually does. Coverage is not
  complete, though: three **gating** `ci.yml` jobs have no `just` equivalent —
  `yaml-lint` (`yamllint .`), `msrv` (`cargo check --all-targets` on the
  toolchain the `msrv` job pins, with `rust-toolchain.toml` removed first —
  see the job for why), and `target-check` (the default and E2E test tiers via
  `cargo test --target x86_64-unknown-linux-musl` and `cargo test --target
  aarch64-unknown-linux-musl`, needing `musl-tools` on a matching-architecture
  Linux host for each leg; the two aarch64-glibc/Windows triplets it used to
  cross-compile-check are now covered by real, executed runs of the `test`
  job below instead — see that job's comment) — so a clean run of every `just`
  target does not by itself guarantee a green CI run; run those three
  directly, or rely on CI, before merging or publishing. `cargo test <name>`,
  the various `cargo install …`/`rustup …` setup commands below, and
  `cargo +nightly fuzz build` also have no dedicated target. Both the `just`
  and plain-`cargo` forms are documented in the sections below; where a `just`
  target exists, `just <target>` is the shorter path. Before any of the
  above, `just check-env` (or `bash scripts/check-env.sh` /
  `pwsh scripts/check-env.ps1` directly) confirms a stable Rust toolchain is
  on `PATH`.

## Build and test

```sh
cargo build
cargo test
```

Or, mirroring CI's `test` job exactly (`cargo build --all-targets` before
`cargo test`):

```sh
just test
```

Run a single test (substring match on the test name) with:

```sh
cargo test <name>
```

The `test` job's matrix runs on `ubuntu-latest`, `windows-latest`,
`macos-latest` (already aarch64 — Apple Silicon), and the GitHub-hosted
arm64 runners `ubuntu-24.04-arm` and `windows-11-arm`, so every aarch64
release target (see README.md's [platform
matrix](README.md#platform-matrix)) gets real, executed test coverage. The
required `target-check` job additionally runs both the default and E2E suites
as statically linked `x86_64-unknown-linux-musl` binaries on `ubuntu-latest`
and `aarch64-unknown-linux-musl` binaries on `ubuntu-24.04-arm`; musl does not
need to be the host libc for those binaries to execute, only a matching CPU
architecture (there is no apt-packaged aarch64-linux-musl cross-compiler, so
that leg builds natively on the arm64 runner rather than cross-compiling from
an x86_64 host — see release.yml's build-artifacts matrix for the same
reasoning). The two arm64 `test`
entries are **required** checks from the start, same as the three
pre-existing entries in this matrix — there is no non-gating grace period
for them (unlike the informational `coverage`/`perf` jobs, which use
`continue-on-error` deliberately); if you administer branch protection,
add `test (ubuntu-24.04-arm)` and `test (windows-11-arm)` to the required
status checks list alongside the existing `test (*)` entries (and, since
`target-check` gained a second matrix leg, `target-check
(aarch64-unknown-linux-musl)` alongside the pre-existing `target-check
(x86_64-unknown-linux-musl)`, if your protection rule lists exact per-leg
contexts rather than the job name alone).

On a Linux development host with `musl-tools` installed, reproduce the musl
job with:

```sh
rustup target add x86_64-unknown-linux-musl
cargo test --target x86_64-unknown-linux-musl
cargo test --target x86_64-unknown-linux-musl --features e2e --test e2e -- --nocapture
```

On an aarch64 Linux development host, substitute `aarch64-unknown-linux-musl`
for the triple above — no separate cross toolchain to install; `musl-tools` on
an aarch64 host already targets aarch64 musl. There is intentionally no
cross-platform `just` recipe for this host-specific toolchain lane. The stress
tier remains in its separate scheduled/manual
workflow rather than extending this required compatibility gate.

Before opening a pull request or publishing directly to `main`, make sure the
same gates CI enforces pass locally — CI treats clippy warnings as errors, so a
clean run is required:

```sh
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all --check
cargo deny check advisories bans licenses sources
```

or, one target per gate:

```sh
just lint
just fmt-check
just deny
```

The root and every public subcommand also have exact through-binary `--help`
snapshots under `fixtures/cli-help/`. If a CLI contract change is intentional,
regenerate them and review the resulting fixture diff:

```sh
UPDATE_CLI_HELP_GOLDEN=1 cargo test --test cli_help
```

In PowerShell, set `$env:UPDATE_CLI_HELP_GOLDEN = "1"` for the test and remove
the variable afterward. The ordinary `cargo test` path compares the built
binary against the committed snapshots and prints this same update instruction
on drift.

## Documentation quality

CI gates two checks, over two different scopes. A spelling check ([`typos`])
gates the whole tracked tree (source, tests, docs, config — anything not
covered by `.gitignore`, so this pipeline's own gitignored `.work/` scratch
tree is never in scope). A link check ([`lychee`]) gates internal relative
paths and section anchors, but only over the tracked documentation set
(`README.md`, `CONTRIBUTING.md`, `SECURITY.md`, `CHANGELOG.md`,
`docs/**/*.md` and `skills/**/*.md`). A separate, non-gating job additionally checks external
`http(s)` URLs over that same documentation set — a real network request in
CI can flake for reasons unrelated to this repo, so it never blocks a merge.

Run the same checks locally before opening a pull request:

```sh
typos .
lychee --offline --config .lychee.toml README.md CONTRIBUTING.md SECURITY.md CHANGELOG.md 'docs/**/*.md' 'skills/**/*.md'
```

or, as one target (`just docs-checks`, running both commands above in
sequence):

```sh
just docs-checks
```

Install both once:

```sh
cargo install typos-cli --locked
cargo install lychee --locked
```

A spelling finding is either a genuine typo (fix the text) or, rarely, a real
domain term (a flag name, event name, crate name) that needs an exclusion —
add it to [`typos.toml`](typos.toml)'s `[default.extend-words]` table, but
only after confirming it is not actually a misspelling. Link-checker
configuration lives in [`.lychee.toml`](.lychee.toml).

[`typos`]: https://github.com/crate-ci/typos
[`lychee`]: https://github.com/lycheeverse/lychee

## End-to-end tests

Beyond the unit and through-the-binary integration tests, a heavier
**end-to-end containment tier** lives in [`tests/e2e.rs`]. It drives the built
`processkit-cli` binary against real, multi-level process trees and proves
ProcessKit's teardown guarantees *from outside* the runner — observing process
liveness through the OS process table (an independent PID probe), not the
runner's own bookkeeping. It covers:

- a leaked grandchild not surviving a **clean** root exit;
- the same guarantee for a **nonzero** root, with the child's exact code
  forwarded unclamped;
- an **abrupt** runner death still reaping the tree (the Windows Job Object
  kill-on-close; the scenario skips loudly on platforms without that
  kernel-enforced guarantee);
- a descendant that **holds the stdout handle** after the root exits not hanging
  the runner (proven with an upper time bound);
- a rapid launch → exit → relaunch storm not misattributing or killing an
  unrelated bystander as **PIDs recycle**;
- `--inherit-stdio` preserving direct input/output and terminal detection in a
  real Windows console and a POSIX pseudo-terminal, while lifecycle JSONL still
  closes cleanly.

The tier is gated behind the `e2e` Cargo feature (with its `e2e_helper` worker),
so it is **off** in the default `cargo test` — those scenarios spawn real process
trees and run longer. Run it explicitly:

```sh
cargo test --features e2e --test e2e -- --nocapture
```

or:

```sh
just e2e
```

`--nocapture` surfaces the explicit `SKIP …` line a scenario prints when its
platform primitive is unavailable. A scenario that would leak a worker on
failure is self-healing: the helper workers self-terminate on a bounded timer,
and the harness never kills by (recyclable) PID. CI runs this tier as a separate
`e2e` job with the same OS matrix as `test` above — `ubuntu-latest`,
`windows-latest`, `macos-latest`, `ubuntu-24.04-arm`, and `windows-11-arm` —
so this is the tier that most directly exercises each platform's containment
mechanism (Job Object / cgroup v2 / process group) on real aarch64 hardware,
not just x86_64.

[`tests/e2e.rs`]: tests/e2e.rs

## Stress tests

The tiers above each prove a functional path: a helper in isolation, one
subcommand's contract through the binary, or containment against a scripted
handful of real processes. [`tests/stress.rs`] covers what none of them can
reach by construction — the invariants that only break when many runs contend
for the two resources *every* run shares, the per-user registry
([`src/registry/mod.rs`](src/registry/mod.rs)) and the per-run control plane
([`src/control/`](src/control/)). It launches dozens of simultaneous `run`
invocations against a single scratch registry directory and drives parallel
`list`/`prune`/`wait`/`inspect`/`cancel`/`kill` clients at them, asserting four
properties a race would break:

- `prune` never reaps a **live** entry — including one belonging to a runner
  still inside its reservation window (the window between creating its lock
  file and publishing its record);
- a registry scan never **loses or duplicates** a record while other runs
  concurrently write and delete their own;
- a control client aimed at an **unreachable or dying** runner refuses with the
  reserved `CONTROL` (103) exit code inside a bounded deadline, instead of
  hanging on a dead endpoint;
- `wait` never **misses** the completion it is watching for, and never
  announces one that has not happened.

Each scenario is a *differential*, not an absence check: it plants entries a
correct `prune` must reap, requires the scanner to actually observe records
appearing and disappearing, aims the same control clients at live runs that
must answer `0`, and checks that `wait` really does block (reporting its own
`WAIT_TIMEOUT`, 112) against a run that is still going. A "never happens"
assertion with nothing forcing the machinery to *do* anything can pass while
proving nothing, so every scenario's docstring also records the temporary
source-level break that was used to confirm the assertion fails when the
invariant does. Keep that up if you add a scenario.

The tier is gated behind the `stress` Cargo feature, so it is **off** in the
default `cargo test`. Run it explicitly:

```sh
cargo test --features stress --test stress -- --nocapture
```

Knobs, all optional environment variables: `PROCESSKIT_STRESS_RUNS` (how many
simultaneous runs a scenario launches, default 24) plus per-scenario
`PROCESSKIT_STRESS_PRUNERS`, `PROCESSKIT_STRESS_CHURN`,
`PROCESSKIT_STRESS_HAMMERS`, `PROCESSKIT_STRESS_WAITERS`, and
`PROCESSKIT_STRESS_SECONDS`. Dial `PROCESSKIT_STRESS_RUNS` down on a small
machine, or up to hunt a race that needs heavier contention.

The scenarios serialize against each other (a process-wide lock inside the
tier), so `--test-threads` does not change what they measure. Every run is
pointed at a scratch registry directory of its own via
`PROCESSKIT_CLI_REGISTRY_DIR` — your own per-user registry is never touched —
and nothing is ever killed by PID: runs are torn down through the control
plane, with the owned process handle as a backstop and a `--timeout` on each
runner so an aborted run leaves nothing behind for long. CI runs this tier as a
separate, **non-gating** `stress.yml` workflow on a weekly schedule and by
manual dispatch — never on push/pull request — so a slow or contended runner
can never block an unrelated PR.

[`tests/stress.rs`]: tests/stress.rs

## Fuzzing

Beyond the grammar-shaped generators the [proptest] property tier drives,
[`fuzz/`] is a [`cargo-fuzz`] tier that explores the parsers of untrusted or
semi-trusted input with unconstrained, coverage-guided bytes. Three targets,
each linking the crate's library target directly (never the binary):

- `registry_record` — the run registry's bytes → parse/validate path
  ([`Registry::scan`]'s per-record guards: JSON, `started_at`, `lock_file`).
- `control_wire` — the control plane's server-side request-line classifier and
  client-side response-line JSON decode (`src/control/mod.rs`).
- `cli_parsers` — the CLI's scalar value parsers, operator-label grammar, and
  raw `--env-file` contents (including invalid UTF-8 and the invariant that a
  rejected entry never repeats its secret value).

Each target ships a small seed corpus under `fuzz/corpus/<target>/`, including
historically found edge cases (a NUL/control byte or a Windows reserved device
name in a registry `lock_file`, a calendar-invalid `started_at` like
`2026-02-31`, and valid/commented/malformed environment and label inputs).

Requires a nightly toolchain (`rustup toolchain install nightly` — the pinned
`stable` from [Prerequisites](#prerequisites) is not enough, since `cargo-fuzz`
needs nightly's sanitizer support) and [`cargo-fuzz`]:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz --locked
```

Run one target for a bounded time, seeded from its committed corpus:

```sh
cd fuzz
cargo +nightly fuzz run registry_record -- -max_total_time=60
```

or, a short bounded smoke run of any target from the repo root (defaults to
`registry_record` for 10 seconds; both are overridable — `just fuzz-smoke
control_wire 30`), useful as a quick local sanity check rather than a real
fuzzing session:

```sh
just fuzz-smoke
```

A crash minimizes to a reproducing input under `fuzz/artifacts/<target>/`; feed
it back to `cargo +nightly fuzz run <target> <path-to-input>` to reproduce.
`cargo +nightly fuzz build` alone (no `run`) is enough to confirm the fuzz crate
still compiles without spending any fuzzing time.

The tier is deliberately **outside** the main crate's lint/build gates:
`fuzz/` is its own crate (`fuzz/Cargo.toml` carries its own empty
`[workspace]`) with its own `Cargo.lock`, so `cargo fmt --all --check` and
`cargo clippy --all-targets --all-features` at the repo root never touch it,
and it needs the nightly toolchain's sanitizer support the rest of the crate
does not. CI runs it as a separate, **non-gating** `fuzz.yml` workflow on a
weekly schedule and by manual dispatch — never on push/pull request — so a
finding never blocks an unrelated PR; see the workflow file for how a crash is
reported.

[proptest]: https://github.com/proptest-rs/proptest
[`fuzz/`]: fuzz
[`cargo-fuzz`]: https://github.com/rust-fuzz/cargo-fuzz
[`Registry::scan`]: src/registry/mod.rs

## Code coverage

CI measures line/region coverage with [`cargo-llvm-cov`] in a dedicated
`coverage` job (ubuntu-latest and windows-latest, so both the `cfg(unix)` and
`cfg(windows)` halves of the code are covered), publishing an HTML report as a
build artifact plus a text summary in the job's step summary. It is
informational only: no coverage-percentage threshold gates the build, and the
job never fails the pipeline, so treat a dip as a signal to look, not a
required fix.

Install it once (the `llvm-tools-preview` rustup component is required):

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --locked
```

Run it locally over the same scope CI measures — the default `cargo test`
tier (unit tests plus the through-the-binary integration tests), excluding the
feature-gated `e2e` tier:

```sh
cargo llvm-cov --open
```

or:

```sh
just coverage
```

`--open` builds an HTML report and opens it in your browser; drop the flag (or
edit the `coverage` target) for a plain terminal summary instead.

[`cargo-llvm-cov`]: https://github.com/taiki-e/cargo-llvm-cov

## Benchmarks

CI publishes a [criterion]-based regression lane to the non-gating `perf`
job's step summary on every push/PR — see [README.md, "Benchmarks"] for what
it covers (internal primitives plus through-the-binary scenarios) and how to
read the results. Each OS also uploads a 90-day `perf-history-<os>` artifact
with median estimates and compares it with the latest successful `main` run.
Changes above 20% produce an Actions warning and a comparison table, but never
gate the build because shared runners are noisy. Run the same benchmark locally:

```sh
cargo bench --features bench
```

or:

```sh
just bench
```

To reproduce the history transformation or compare two saved summaries:

```sh
python scripts/criterion_history.py summarize target/criterion current.json
python scripts/criterion_history.py compare baseline.json current.json \
  --threshold-percent 20 --markdown comparison.md
python -m unittest scripts/tests/test_criterion_history.py
```

Release package-manager manifests have a separate standard-library-only test:

```sh
python -m unittest scripts/tests/test_generate_package_manifests.py
```

[criterion]: https://github.com/bheisler/criterion.rs
[README.md, "Benchmarks"]: README.md#benchmarks

## Mutation testing

CI runs a scheduled, non-gating [`cargo-mutants`] tier
(`.github/workflows/mutants.yml`) that reruns the crate's default `cargo test`
tier once per artificially introduced defect ("mutant") across `src/`. A
mutant the suite doesn't catch — a *survivor* — is a direct pointer to a gap
the coverage numbers above cannot see: a line executing only proves it ran,
not that a test would notice if its behavior changed.

Configuration lives in [`.cargo/mutants.toml`](.cargo/mutants.toml) —
cargo-mutants' one auto-discovered config path (unlike `deny.toml`/`cliff.toml`,
it has no root-level alternative it searches, so this is not a repo
convention choice). It scopes mutation to `src/**/*.rs`, excluding the thin
`src/main.rs` entry point, the feature-gated helper binaries under `src/bin/`
(the `e2e` and `bench` tiers' worker binaries — test/bench harnesses, not
library logic), and the Windows-only `src/win_security.rs` (the CI job runs
on `ubuntu-latest` only, so that module never compiles into the build it
mutates); see the file's own comments for the reasoning behind each
exclusion.

This is by far the most expensive tier in this repo — a full run reruns the
whole test suite once per mutant — so, like `fuzz.yml`, it is scheduled
weekly and manual-dispatch only, never on push/pull request. CI splits the
scoped tree across 8 parallel shards (cargo-mutants' `--shard k/n`; see
`.github/workflows/mutants.yml`) so that each job finishes inside a
GitHub-hosted runner's job time limit.

Install it once:

```sh
cargo install cargo-mutants --locked
```

Running the full scoped tree locally (`cargo mutants`, no arguments) is a
multi-hour, single-threaded command (hundreds of mutants, each a rebuild plus
a full `cargo test` run) — not something to run start-to-finish on a whim.
Prefer scoping to what you're actually iterating on: a single file while
working on its tests,

```sh
cargo mutants --file src/hash.rs
```

or one of CI's shards, to sample the full scoped tree without waiting for
all of it:

```sh
cargo mutants --shard 0/8
```

The `just mutants` target forwards any arguments straight to `cargo mutants`,
so both look like:

```sh
just mutants --file src/hash.rs
just mutants --shard 0/8
```

Results land under `mutants.out/`: `missed.txt` lists survivors, `caught.txt`/
`timeout.txt`/`unviable.txt` the other outcomes, and `logs/` the per-mutant
build/test output. CI publishes the same summary to the job's step summary
and uploads each shard's directory as its own `mutants-out-shard-N` artifact.

[`cargo-mutants`]: https://mutants.rs

## Conventions

- **Formatting** is governed by `rustfmt` (run `cargo fmt`); non-Rust files
  follow [`.editorconfig`](.editorconfig) (LF line endings, final newline). Do
  not reformat code you are not changing.
- **Dependencies** — every entry in [`Cargo.toml`](Cargo.toml) carries an inline
  comment explaining *why* it is there; pin major versions and enable only the
  features you use. `Cargo.lock` is committed for reproducible builds.
- **Commit subjects** are conventional-commit style (`type(scope): summary`) —
  they feed the changelog auto-fill via [`cliff.toml`](cliff.toml).
- **Language** — write source, comments, documentation, configuration, commit
  messages, and all other repository artifacts in English.
- **Publishing** — contributors and automated services use branches and pull
  requests; pushing directly to `main` is reserved for the repository owner.
- **Comments explain the *why*, not the *what*.** The code already states what it
  does; a comment earns its place by recording the non-obvious reason — a
  workaround, a wire contract, a performance trade-off.

## Changelog

Every user-visible change ships its [`CHANGELOG.md`](CHANGELOG.md) entry in the
same change set, under `## [Unreleased]`. Write the bullet for a consumer of the
crate, not the implementer. Pure internal refactors are exempt.

## Contributions and direct publishing

Keep changes focused and ensure CI (fmt, clippy, build/test on Linux, Windows,
macOS, cargo-deny, MSRV, typos, and the internal-docs link check) passes after
each pull request or direct publication to `main`.
