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

## Build and test

```sh
cargo build
cargo test
```

Run a single test (substring match on the test name) with:

```sh
cargo test <name>
```

Before opening a pull request or publishing directly to `main`, make sure the
same gates CI enforces pass locally — CI treats clippy warnings as errors, so a
clean run is required:

```sh
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo deny check advisories bans
```

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

`--nocapture` surfaces the explicit `SKIP …` line a scenario prints when its
platform primitive is unavailable. A scenario that would leak a worker on
failure is self-healing: the helper workers self-terminate on a bounded timer,
and the harness never kills by (recyclable) PID. CI runs this tier as a separate
`e2e` job on Linux, Windows, and macOS.

[`tests/e2e.rs`]: tests/e2e.rs

## Fuzzing

Beyond the grammar-shaped generators the [proptest] property tier drives,
[`fuzz/`] is a [`cargo-fuzz`] tier that explores the parsers of untrusted or
semi-trusted input with unconstrained, coverage-guided bytes. Three targets,
each linking the crate's library target directly (never the binary):

- `registry_record` — the run registry's bytes → parse/validate path
  ([`Registry::scan`]'s per-record guards: JSON, `started_at`, `lock_file`).
- `control_wire` — the control plane's server-side request-line classifier and
  client-side response-line JSON decode (`src/control.rs`).
- `cli_parsers` — the CLI's `--timeout`/`--grace`, `--require-exit-code-band`,
  and `--env` value parsers (`src/cli.rs`).

Each target ships a small seed corpus under `fuzz/corpus/<target>/`, including
historically found edge cases (a NUL/control byte or a Windows reserved device
name in a registry `lock_file`, a calendar-invalid `started_at` like
`2026-02-31`).

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
[`Registry::scan`]: src/registry.rs

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

`--open` builds an HTML report and opens it in your browser; drop the flag for
a plain terminal summary instead.

[`cargo-llvm-cov`]: https://github.com/taiki-e/cargo-llvm-cov

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
macOS, cargo-deny, and MSRV) passes after each pull request or direct
publication to `main`.
