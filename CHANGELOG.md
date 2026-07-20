# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Add entries to `[Unreleased]` as you work — manual bullets always win over the
git-cliff auto-fill (config: `cliff.toml`). On release, promote `[Unreleased]`
to a dated version section.

## [Unreleased]

### Added
- Initial project skeleton.
- Command-line surface: the `run`, `inspect`, `cancel`, and `kill` subcommands
  are parsed and validated, including `run`'s verbatim `-- <program> <args...>`
  tail.
- `run` execution: launches the program shell-free inside a ProcessKit
  `ProcessGroup` the runner owns (in `--cwd`, defaulting to the current
  directory), echoes the child's stdout/stderr live through ProcessKit's pipes
  (pipe + echo, so the child sees no TTY — colors/progress bars may degrade),
  and forwards the child's exit code exactly. Runner-own failures use the
  reserved `100..=119` band (`SPAWN`/`BACKEND`/`INTERNAL`). When `run` returns,
  the container is torn down by the group's kernel-backed kill-on-drop, so leaked
  descendants do not survive. `--create-no-window` is proxied to
  `Command::create_no_window()` (default off).
- Documented runner exit-code contract (`docs/exit-codes.md`) that keeps the
  runner's own failures in a reserved code band, separate from the child's
  exit code.
- Dependencies on `processkit` (the containment backbone), `tokio` (its async
  runtime), and `clap` (CLI parsing).

### Changed
- `inspect`, `cancel`, and `kill` remain unimplemented and still exit with the
  runner-range "not implemented" code; `run` no longer does.

### Fixed
-

[Unreleased]: https://github.com/ZelAnton/processkit-cli/commits/HEAD
