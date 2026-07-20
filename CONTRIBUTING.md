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

Before opening a pull request or publishing directly to `main`, make sure the
same gates CI enforces pass locally — CI treats clippy warnings as errors, so a
clean run is required:

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
