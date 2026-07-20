# Contributing to processkit-cli

Thanks for your interest in improving **processkit-cli**.

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

Before opening a pull request, make sure the same gates CI enforces pass
locally — CI treats clippy warnings as errors, so a clean run is required:

```sh
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo deny check advisories bans
```

## Conventions

- **Formatting** is governed by `rustfmt` (run `cargo fmt`); non-Rust files
  follow [`.editorconfig`](.editorconfig) (LF line endings, final newline). Do
  not reformat code you are not changing.
- **Dependencies** — every entry in [`Cargo.toml`](Cargo.toml) carries an inline
  comment explaining *why* it is there; pin major versions and enable only the
  features you use. `Cargo.lock` is committed for reproducible builds.
- **Commit subjects** are conventional-commit style (`type(scope): summary`) —
  they feed the changelog auto-fill via [`cliff.toml`](cliff.toml).
- See [`AGENTS.md`](AGENTS.md) for the full, authoritative set of conventions
  (code style, dependency management, supply chain/MSRV, version control).

## Changelog

Every user-visible change ships its [`CHANGELOG.md`](CHANGELOG.md) entry in the
same change set, under `## [Unreleased]`. Write the bullet for a consumer of the
crate, not the implementer. Pure internal refactors are exempt.

## Pull requests

- Keep changes focused; unrelated cleanups belong in their own PR.
- Ensure CI (fmt, clippy, build/test on Linux, Windows, and macOS, cargo-deny,
  MSRV) passes.
- Fill in the pull-request checklist.
