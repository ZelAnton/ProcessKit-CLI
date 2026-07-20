# Security Policy

## Supported versions

Security fixes are applied to the latest released version of **processkit-cli**.
Older versions are not maintained — upgrade to the latest release to receive
fixes.

## Reporting a vulnerability

**Do not open a public issue for security vulnerabilities.**

Report privately through GitHub's
[private vulnerability reporting](https://github.com/ZelAnton/processkit-cli/security/advisories/new)
(repository **Security → Advisories → Report a vulnerability**). If that is
unavailable, contact the maintainer listed on the
[ZelAnton](https://github.com/ZelAnton) profile.

Please include:

- a description of the vulnerability and its impact;
- steps to reproduce (a minimal proof of concept is ideal);
- affected version(s).

You can expect an initial acknowledgement within a few days. Once a fix is
ready, a patched release is published to crates.io and the advisory is disclosed.

## Automated scanning

There is **no GitHub CodeQL analysis** for this repository — CodeQL ships no Rust
extractor, so it cannot analyze a Rust codebase. Instead the supply chain is
guarded by Rust-native tooling:

- **[cargo-deny](https://embarkstudios.github.io/cargo-deny/)** runs in CI on
  every pull request and every push to `main` (the `cargo-deny` job in
  [`.github/workflows/ci.yml`](.github/workflows/ci.yml), driven by
  [`deny.toml`](deny.toml)). It runs `cargo deny check advisories bans`: the
  dependency tree is checked against the [RustSec](https://rustsec.org/) advisory
  database and the build fails on a security advisory, a yanked crate, or a
  wildcard version requirement. (License and source gating are available in
  cargo-deny but are not enabled here.)
- **[Dependabot](.github/dependabot.yml)** opens weekly pull requests to keep the
  `cargo` dependencies (and the pinned GitHub Actions) current, so advisory fixes
  land promptly.
