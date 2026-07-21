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
- `run` now enforces `--timeout` and `--grace` and handles `Ctrl-C`, all as
  **distinguishable** endings that share one teardown path. A `--timeout` that
  elapses exits with the reserved `TIMEOUT` code (106); a `Ctrl-C` cancel exits
  with the reserved `CANCELLED` code (107) — each distinct from the other and from
  a forwarded child code — with an explanatory line on stderr. Both first ask the
  tree to stop, wait out `--grace`, then let the owning container's kill-on-drop
  hard-tear-down the whole tree, so no descendant survives either ending.
  `--timeout`/`--grace` accept a small duration grammar (`ms`/`s`/`m`/`h`, integer,
  default `s`); a malformed value is a usage error (100). On Windows, where the
  ProcessKit kernel has no soft-terminate tier yet, no soft signal is sent — the
  grace window elapses and the Job Object is then killed atomically, and the runner
  reports this honestly rather than implying a graceful stop. (The machine-readable
  JSONL form of these outcomes lands with the event schema.)
- Documented runner exit-code contract (`docs/exit-codes.md`) that keeps the
  runner's own failures in a reserved code band, separate from the child's
  exit code, and now assigns `TIMEOUT` (106) and `CANCELLED` (107).
- Versioned JSONL event schema (v1): `run` now writes a stream of lifecycle
  events to the `--jsonl` file — one JSON object per line, each with a
  `schema_version`, and never to stdout. The stream covers `run_started` (run id,
  root PID, containment mechanism, abrupt-owner-death cleanup scope, working
  directory), `members_snapshot`,
  `root_exited`, the `cleanup_started` / `cleanup_finished` teardown pair,
  `timeout` / `cancelled`, launch and container errors, and a terminal
  `runner_exit` that preserves the child's own code even on the runner's own
  failure — so a child's code is never lost or aliased. The command line is
  redacted by default (raw argv only under `--argv-raw`; the redaction hash and
  worker-shape hint are reserved fields), and member snapshots are PID-only with
  the richer per-member fields declared but absent until ProcessKit-rs ships them.
  Normative reference in `docs/schema.md`; golden sample stream published at
  `fixtures/schema/v1/events.jsonl` and gated by a golden test. `--run-id` and
  `--argv-raw` are now consumed.
- Bounded output capture (`--capture-dir <dir>`): the child's stdout and stderr are
  teed into `<dir>/stdout.log` and `<dir>/stderr.log` alongside the unchanged live
  echo, kept separate per stream. A new `output_captured` JSONL event records, for
  each stream, the file path, a full byte counter, a SHA-256 of the captured bytes
  (the same digest primitive as the argv fingerprint), and an explicit truncation
  flag — so a consumer distinguishes "captured in full" from "clipped at the limit"
  without inferring it from the file's size. The capture is bounded by ProcessKit's
  byte-capped `OutputBufferPolicy` (the pump's in-flight memory) plus a per-stream
  file ceiling; the runner adds no draining or limiting of its own, and the
  held-descriptor teardown bound is preserved (a descendant keeping an output handle
  open past the root's exit cannot hang the runner). A run without `--capture-dir`
  is byte-for-byte unchanged (no files, no event). Additive schema v1 change,
  reflected in `docs/schema.md` and the golden fixture.
- Control-plane `cancel` and `kill` subcommands: `cancel --run-id <id>` and
  `kill --run-id <id>` reach the live runner over the same local transport and
  registry discovery as `inspect` (by `run_id`, never a PID) and end the run. `cancel`
  runs the runner's **shared** soft-stop → grace → hard-kill teardown — the same path
  a `--timeout` or a `Ctrl-C` drives, honest Windows hard-kill fallback included — and
  the run exits with the new reserved code `CONTROL_CANCELLED` (108); `kill` hard-kills
  the whole tree immediately (no soft stop, no grace) and the run exits with
  `CONTROL_KILLED` (109). Both are distinguishable from a Ctrl-C, a timeout, and each
  other by exit code *and* in the JSONL stream: `cancel` writes a `cancelled` event
  with `source` `control_cancel`, `kill` writes a new `killed` event with `source`
  `control_kill`, and each closes with a terminal `runner_exit` carrying the matching
  `source` — so an external observer reading `--jsonl` sees the external command, not
  just the control client. The kill scope is only the target run's ProcessKit
  container (discovered via the registry); nothing is ever killed by executable name.
  The wire protocol gains the two verbs without reshaping its one-request/one-JSON-line
  framing, each answered with a `{"accepted":…,"action":…,"run_id":…}` ack, and an
  unreachable/stale runner is the same bounded `CONTROL` (103) failure as `inspect`.
  Additive schema v1 change (new `source` values and the `killed` event), reflected in
  `docs/control-plane.md`, `docs/schema.md`, `docs/exit-codes.md`, and the golden
  fixture.
- Abrupt runner-death hardening and proof: every spawned command opts into
  ProcessKit's public parent-death primitive. The versioned `run_started` event
  now reports the actual surviving guarantee as `abrupt_cleanup` (`whole_tree`
  on Windows, `direct_child_only` on Linux, `none` on macOS/other Unix), and the
  E2E tier force-kills the runner with a live child/grandchild to verify each
  platform's behavior without unsafe kill-by-PID cleanup.
- Dependencies on `processkit` (the containment backbone), `tokio` (its async
  runtime), `clap` (CLI parsing), and `serde` / `serde_json` (the JSONL event
  schema).
- Prebuilt release binaries: the manual `release.yml` workflow now fans out a
  downstream `build-artifacts` matrix that builds a `--release` binary for
  Windows, Linux, and macOS across x86_64 and aarch64 — plus a statically linked
  `x86_64-unknown-linux-musl` build for minimal/container images — and attaches
  each archive to the same GitHub Release. It runs strictly after the existing
  crates.io publish + tag, so the release ordering is unchanged and there is still
  a single release path; `cargo install processkit-cli` remains a first-class
  install. `README.md` gains an Installation section with a platform matrix that
  states the actual kernel container mechanism reported per platform (Job Object
  on Windows, cgroup v2 on Linux, POSIX process group on macOS/other Unix).

### Changed
- The control plane's three clients — `inspect`, `cancel`, and `kill` — all reach a
  live runner over the local transport now; no subcommand returns the runner-range
  "not implemented" code any longer.
- `run` now consumes every flag it parses: `--jsonl` (the JSONL event stream) and
  `--capture-dir` (bounded output capture) are both wired up.

### Fixed
- Unix control sockets now use a short owner-only temporary directory instead of
  inheriting the registry's full path, so deeply nested macOS CI/workspace paths
  cannot exceed `sockaddr_un::sun_path` and silently disable `inspect`.

[Unreleased]: https://github.com/ZelAnton/processkit-cli/commits/HEAD
