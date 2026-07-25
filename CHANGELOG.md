# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Add entries to `[Unreleased]` as you work — manual bullets always win over the
git-cliff auto-fill (config: `cliff.toml`). On release, promote `[Unreleased]`
to a dated version section.

## [Unreleased]

### Added
- A concurrency stress test tier (`tests/stress.rs`, `stress` Cargo feature)
  covering the invariants that only break when many runs contend for the two
  resources every run shares — the per-user registry and the per-run control
  plane. It launches dozens of simultaneous `run` invocations against one
  registry directory and drives parallel `list`/`prune`/`wait`/`inspect`/
  `cancel`/`kill` clients at them, asserting that `prune` never reaps a live
  entry (including one still inside its reservation window), that a registry
  scan never loses or duplicates a record under concurrent writes and
  deletions, that a control client aimed at an unreachable or dying runner
  refuses with `CONTROL` (103) inside a bounded deadline instead of hanging,
  and that `wait` never misses — or invents — a completion. Every scenario
  carries a positive control, so none of those "never" assertions can pass
  vacuously. Dev-only tooling: off by default, like the `e2e` and `bench`
  tiers, so it never affects a plain `cargo build`/`cargo test`/`cargo
  publish`; CI runs it as a separate, non-gating scheduled `stress.yml`
  workflow (see CONTRIBUTING.md, "Stress tests"). No runtime behavior, CLI
  surface, exit code, or event-schema change.

### Changed
-

### Fixed
-

## [0.3.0] - 2026-07-25

### Added
- **New `run --detach` flag: start a run and let go.** The call re-spawns the CLI
  detached — a new session on Unix (`setsid`), `DETACHED_PROCESS` on Windows, `null`
  stdio either way — and returns as soon as that copy has *provably* started the run,
  instead of staying the runner's parent for its whole duration. "Provably" is an
  observation, not an assumption: the call waits until the detached runner's
  `run_started` event is readable in `--jsonl`, which it writes only after creating the
  container, publishing the registry record, and spawning the child — so on return the
  run is already discoverable with `list`, reachable with `inspect`/`cancel`/`kill`,
  and waitable with `wait` (and the run id is readable from the events file even when
  the runner generated it). The detached copy runs the ordinary `run` path unchanged —
  same container, same teardown, same JSONL stream — so detaching adds a spawn and a
  handshake, not a second lifecycle. A caller that *captures* the launch command's
  output sees end-of-file when the call returns rather than when the run ends — the
  detached runner is left holding none of the caller's pipes (on Windows this needed
  an explicit `HANDLE_FLAG_INHERIT` clear, since `CreateProcess`'s handle inheritance
  is all-or-nothing). **The exit code changes meaning under this flag,
  and only under it:** it reports whether the run *started* — `0` once it has, never
  the child's own code, which stays in the terminal `runner_exit` event where a
  detached caller can still observe it. A start that fails is never reported as
  success: the detached runner's own reserved-band code is passed through unchanged
  (a missing program is still `SPAWN` 101, an unusable container still `BACKEND` 102,
  an unwritable `--jsonl` still `SETUP` 111 — reported here before anything is
  spawned), so **no new exit code was minted** and `113`–`119` stay reserved. There is
  no live echo while detached — the detached runner reuses `--no-echo`'s discarding
  sinks rather than a second suppression path — while `--capture-dir`,
  `--idle-timeout`, and the JSONL stream keep observing the child exactly as in the
  foreground, and `--jsonl` stays required. It conflicts at parse time with
  `--inherit-stdio` and `--inherit-stdin` (nothing interactive survives detaching).
  On Windows, pair it with `--create-no-window` for a console child: the detached
  runner has no console to lend, so the OS gives the child a fresh one. `probe --json`
  advertises the new surface automatically (`run:--detach`). See README.md, "Detached
  runs", docs/exit-codes.md, "Detached runs", and docs/integration.md, §2.
- **New `wait --run-id <id> [--timeout <duration>]` subcommand**: block until a run
  recorded in the per-user registry is no longer live. It closes the one supervision
  gap the control plane left open — a supervisor that did *not* start the run (an
  adapter that restarted, a cleanup step, anything holding only a `run_id`) has no
  child process to wait on, and previously had to hand-roll a polling loop around
  `inspect` and read run lifetime out of `CONTROL` refusals, which conflate "I could
  not reach it" with "it finished". `wait` is registry-only: it opens the registry
  read-only (like `list`/`prune`, so waiting never creates the directory or touches
  its permissions), never connects to the run's control transport, never ends or
  disturbs the run, and needs no control endpoint — so a run whose transport never
  came up is still waitable. Because the liveness signal is an OS advisory lock with
  no event to subscribe to, it waits by honest periodic probing rather than pretending
  to be notified. Three outcomes, by exit code: the run is over (`0`), the wait's own
  `--timeout` elapsed with the run still live (the **new reserved code
  `WAIT_TIMEOUT` = 112**, see *Exit codes* below), or the `run_id` is ambiguous —
  more than one live run registered under it, so there is no single run to wait for —
  which reuses the same `CONTROL` (103) refusal `inspect`/`cancel`/`kill` give.
  Nothing is printed on success; the exit code is the whole answer. `--timeout` reuses
  `run --timeout`'s exact parser and grammar (including its rejection of a degenerate
  `0`), and omitting it blocks indefinitely. **One deliberate design choice callers
  must plan for:** a run that exits cleanly deletes its own registry entry, so an
  *unknown* `run_id` is indistinguishable from one that already finished and was
  cleaned up — both exit `0`. That keeps the ordinary "the run finished while I was
  starting up" race from becoming a hard error, at the price that a typo'd `run_id`
  also returns success immediately: `wait`'s `0` means "not running", never "existed
  and completed". `probe --json` advertises the new surface automatically (`wait`,
  `wait:--run-id`, `wait:--timeout`). See README.md, "Command interface", and
  docs/registry.md, "Waiting — `wait`".
- **New reserved exit code `WAIT_TIMEOUT` (112)**, taking the next free slot after
  `SETUP` (111) in the reserved `100`–`119` band (`113`–`119` remain reserved). It is
  minted only by `wait`, and only when *the waiter's* `--timeout` elapsed while the
  run was still live — the run itself was never touched and is still going.
  Deliberately not `TIMEOUT` (106), which means the opposite (the *runner* enforced a
  deadline and tore the child's tree down), and not `CONTROL` (103), since the run was
  resolved unambiguously and found healthy. See docs/exit-codes.md, "A waiter's
  deadline is not a run's deadline".
- **Windows: `Ctrl-Break`, console close, logoff, and system shutdown now end a run
  through the full cancel teardown** instead of the OS's default handling silently
  ending the runner. The console-control events `CTRL_BREAK_EVENT`,
  `CTRL_CLOSE_EVENT`, `CTRL_LOGOFF_EVENT`, and `CTRL_SHUTDOWN_EVENT` (caught via
  `tokio::signal::windows`, the same `SetConsoleCtrlHandler` mechanism `Ctrl-C`
  already used) join `Ctrl-C` in the same race, so they get the same soft-stop →
  `--grace` → hard-kill teardown, the same terminal JSONL events (`cancelled`,
  `cleanup_started`, `cleanup_finished`, `runner_exit`), the same registry-entry
  removal, and the same reserved `CANCELLED` (107) exit. Previously the OS's default
  handling terminated the runner outright on all four: the events were never
  written, the registry entry was left behind stale, and the container was never
  explicitly killed — the ending went unreported to any observer of the event stream
  or registry, even though the tree itself was not left orphaned: Windows already
  reaps the *whole* tree on abrupt owner death (`abrupt_cleanup: whole_tree`, closing
  the runner's last Job Object handle), unlike Linux's direct-child-only `PDEATHSIG`
  reap. Which event arrived is reported honestly rather
  than flattened onto a keyboard interrupt: the `cancelled` event's `source` gained
  the additive values **`ctrl_break`**, **`ctrl_close`**, **`ctrl_logoff`**, and
  **`ctrl_shutdown`** alongside `ctrl_c` (`schema_version` unchanged — a new value
  of an existing string field), and the stderr line names the event
  (`Ctrl-Break`/`console close`/`logoff`/`system shutdown`). All four keep the one
  `CANCELLED` (107) code, the same class of ending. `CTRL_CLOSE_EVENT` carries an
  OS-imposed termination deadline (about 5 seconds): the runner caps the
  *effective* `--grace` for that trigger alone to a budget comfortably inside that
  window (a longer request degrades to the shorter, honest wait — and the
  `cancelled` event's `grace_ms` reports this effective value, not the raw request
  — rather than risk the OS killing the runner mid-teardown, before the terminal
  events are even written); `Ctrl-Break`/logoff/shutdown carry no such matching
  deadline this runner can honestly bound, so they are left uncapped. See
  README.md, "Timeouts, cancel, and grace", and docs/schema.md / docs/exit-codes.md.
- **Unix: `SIGTERM` and `SIGHUP` now end a run through the full cancel teardown**
  instead of killing the runner where it stands. The standard external stop — a plain
  `kill <pid>`, a `systemctl stop`, a cancelled CI job, a supervisor's shutdown
  timeout — and a hung-up controlling terminal join `Ctrl-C` in the same race, so they
  get the same soft-stop → `--grace` → hard-kill teardown, the same terminal JSONL
  events (`cancelled`, `cleanup_started`, `cleanup_finished`, `runner_exit`), the same
  registry-entry removal, and the same reserved `CANCELLED` (107) exit. Previously
  their default disposition terminated the runner outright: the events were never
  written, the registry entry was left behind stale, and — the guarantee that matters —
  the container was never explicitly killed, so on Linux only the direct child was
  reaped (`PDEATHSIG`) and on macOS/BSD nothing was. Which signal arrived is reported
  honestly rather than flattened onto a keyboard interrupt: the `cancelled` event's
  `source` gained the additive values **`sigterm`** and **`sighup`** alongside `ctrl_c`
  (`schema_version` unchanged — a new value of an existing string field), and the
  stderr line names the signal. All three keep the one `CANCELLED` (107) code, the same
  class of ending. A signal the environment deliberately neutralized before launching
  the runner (`SIG_IGN`, as `nohup` does to `SIGHUP`) is left alone rather than
  un-ignored — `nohup processkit-cli run …` keeps surviving a hangup, and nothing is
  lost, because an ignored signal would not have stopped the runner either. Windows was
  left unchanged by *this* entry — its `Ctrl-Break`/console-close/logoff/shutdown
  handling is covered by the Windows entry above, which now joins `Ctrl-C` in the same
  race. See README.md, "Timeouts, cancel, and grace", and docs/schema.md /
  docs/exit-codes.md.
- `run` gained `--idle-timeout <duration>`, a deadline on child **silence** for the
  stuck-worker case (a child that is alive but has long stopped producing output).
  The deadline is re-armed on every chunk of the child's output, so a child that
  keeps talking is never reaped no matter how long it runs — only one that goes quiet
  past the window is. An idle expiry reuses the existing `TIMEOUT` (106) exit and the
  same soft-stop → grace → hard-kill teardown as `--timeout`; the two are told apart
  by a new always-present `reason` field on the `timeout` JSONL event (`overall` vs
  `idle`), so `schema_version` is unchanged (additive field). Same duration grammar
  as `--timeout`, including its parse-time rejection of `0` (see the `Changed` entry
  below); a malformed value is a `USAGE` (100) parse-time error. It
  needs the runner's output pump, so it conflicts with `--inherit-stdio` at parse
  time (like `--capture-dir`) but composes with `--capture-dir`. The new flag appears
  in the `probe` surface tokens automatically. See README.md, "Timeouts, cancel, and
  grace", and docs/schema.md / docs/exit-codes.md.
- `run` resource-limit flags `--max-memory <size>`, `--max-processes <n>`, and
  `--cpu-quota <cores>`, mapping onto ProcessKit's whole-tree `ProcessGroupOptions`
  caps (the `processkit` dependency now enables its `limits` feature). Enforcement
  needs a real container — a Windows Job Object or a Linux cgroup v2 at the real
  hierarchy root — so where a cap cannot be applied (macOS/BSD and the Linux
  process-group fallback; a cgroup v2 that is unenforceable under
  systemd/containers/typical CI) the run fails fast **before** the child is spawned:
  it now emits the previously reserved **`limit_hit`** JSONL event (naming
  `memory`/`processes`/`cpu`) and exits with `BACKEND` (102), rather than running
  silently unbounded. A nonsensical value (`--max-memory 0`, a non-positive/non-finite
  `--cpu-quota`) is a `USAGE` (100) parse-time error. The new flags appear in the
  `probe` surface tokens automatically; `schema_version` is unchanged (the
  `limit_hit` shape was already fixed in v1). See README.md, "Resource limits", and
  docs/schema.md / docs/exit-codes.md.
- Shell completions (bash/zsh/fish/PowerShell/Elvish) and man pages, generated
  from the live `clap` CLI definition by a new `build.rs` at build time and
  attached to every release archive under `completions/` and `man/man1/` (see
  README.md, "Shell completions and man pages"). Build-time generation, not a
  CLI subcommand, so the binary's own runtime surface — and the `probe`
  compatibility report a consumer's preflight checks — are unchanged.
- `docs/integration.md`: a consumer/adapter integration guide walking through
  the fail-closed `probe` preflight, the recommended `run` invocation, reading
  the JSONL event stream, control-plane supervision (`inspect`/`cancel`/
  `kill`), registry housekeeping (`list`/`prune`), and typical error modes —
  linking the existing normative documents rather than duplicating them.
- A [criterion](https://github.com/bheisler/criterion.rs)-based benchmark tier
  (`benches/`, `bench` Cargo feature) covering incremental SHA-256
  (`src/hash.rs`), bounded-capture `absorb` (`src/capture.rs`), the argv hint
  classifier (`src/events.rs`), and two through-the-binary scenarios — echo
  overhead (direct vs. under `run`, with and without `--capture-dir`) and
  startup latency (call to `run_started`) — plus a non-gating CI `perf` job
  that publishes results to the step summary (see README.md, "Benchmarks").
  Dev-only tooling: off by default, like the `e2e` tier, so it never affects a
  plain `cargo build`/`cargo test`/`cargo publish`. `StreamCapture` and
  `classify_hint` are now `pub` (still `#[doc(hidden)]`, no semver guarantee)
  so the new tier can reach them directly, matching this crate's documented
  "future benchmarks reach internal primitives directly" design
  (`docs/architecture.md`, "Target structure").

- `run` gained `--capture-max-bytes <size>`, a per-stream ceiling for
  `--capture-dir`'s bounded transcript files, replacing the previously
  hard-coded 8 MiB constant with a configurable one (same grammar as
  `--max-memory`: a byte count with an optional binary unit — `1048576`,
  `512k`, `256m`, `2g`; a malformed value is a `USAGE` (100) parse-time error).
  Omitting the flag keeps the prior 8 MiB default, so a bare `run`/`run
  --capture-dir` is byte-for-byte unchanged; the `output_captured` event's
  shape and its `truncated` flag's meaning are unaffected. The pump's
  separate in-flight line-assembly ceiling (`CAPTURE_INFLIGHT_MAX_BYTES`)
  stays an independent constant, not derived from this flag (see
  `src/capture.rs`). Appears in the `probe` surface tokens automatically. See
  README.md, "Bounded output capture".

- `run` gained `--no-echo`, an opt-in that suppresses only the runner's own live
  retransmission of the child's stdout/stderr onto its own stdout/stderr. The pipe
  and the output pump stay wired exactly as without the flag: `--capture-dir`
  still receives the child's bytes in full through the same tee, `--idle-timeout`
  still re-arms on every observed chunk, and the JSONL event stream is unaffected
  — only the live echo write is skipped. Meant for an embedding orchestrator that
  reads results from `--jsonl`/`--capture-dir` and finds the child's raw output,
  interleaved with its own, pure noise. Conflicts with `--inherit-stdio` at parse
  time (like `--capture-dir` and `--idle-timeout`), since that mode runs no pump
  to suppress in the first place. Without `--no-echo`, nothing changes: the live
  echo behaves exactly as before. Appears in the `probe` surface tokens
  automatically (`run:--no-echo`). See README.md, "Standard I/O" and "Bounded
  output capture".

- **New `prune --dry-run` flag: preview a reap without deleting anything.**
  `Registry::preview_prune` (`src/registry.rs`) runs the exact same two-pass scan and
  the exact same `probe_for_prune` liveness classification a real `prune` uses, but
  never calls `fs::remove_file` — a confirmed-stale verdict releases its
  probe-acquired lock immediately (there is nothing to reclaim it for) and records
  the candidate instead of reaping it, so the aggregate tally it returns is exactly
  what a following, untouched `prune` pass over the same registry state would
  report. Without `--json` it lists each confirmed-stale candidate (a paired entry's
  `run_id`/`started_at`, or an orphaned lock's file name) followed by a "would
  prune …" summary line; with `--json` it prints the same
  `pruned`/`live`/`unprobed`/`orphaned_locks` fields `prune --json` already does,
  plus an additional `candidates` array tagged `"kind":"entry"` or
  `"kind":"orphaned_lock"`. `prune` without `--dry-run` is byte-for-byte unchanged.
  Appears in the `probe` surface tokens automatically (`prune:--dry-run`). See
  README.md, "Command interface", and docs/registry.md, "Reaping — `prune`".

### Changed
- `run --timeout 0` and `run --idle-timeout 0` are now rejected at parse time
  (`USAGE`, exit `100`) instead of arming an already-elapsed deadline that tore
  the child down immediately after spawn — almost certainly an operator typo,
  never a useful deadline in its own right. This mirrors the existing
  "degenerate cap" rejection `--max-memory 0`/`--max-processes 0`/`--cpu-quota
  0` already receive. `--grace 0` is unaffected and stays legal ("no pause"
  between the soft stop and the hard kill is a real, useful setting). See
  README.md, "Timeouts, cancel, and grace".
- The JSONL `members_snapshot` event and the control-plane `inspect` snapshot now
  populate the enriched per-member fields (`ppid`, executable `name`,
  `start_time`) from ProcessKit's `ProcessGroup::members_info()` (shipped in
  processkit 2.3.2), instead of always emitting `null`. Each field stays
  independently nullable — `members_info()` itself reports a field `null`
  wherever the platform can't read it (the "bare" BSDs report none of them) — and
  `start_time` is an opaque, platform-specific start-time token rendered as its
  decimal string, not a wall-clock timestamp (see `docs/schema.md`, "Enriched
  member fields"). Filling a field the v1 schema always declared but reserved
  `null` is a non-breaking change (`schema_version` unchanged).
- The crate is now a thin binary over an internal library target (`src/lib.rs`,
  `processkit_cli`): every module moved into the library, and `src/main.rs` only
  parses argv and dispatches into it. This is purely a build-structure change —
  the CLI flags, subcommands, exit codes, and JSONL `schema_version` are
  byte-for-byte unchanged. The library is **not** a stable public API (every
  module is `#[doc(hidden)]` and exempt from semantic versioning); it exists only
  so the crate's own test, fuzz, and benchmark tiers can reach the runner's
  internals directly. The supported compatibility surface remains the binary's.

### Fixed
- The control-plane wire protocol now reads its one request/response line under an
  explicit byte ceiling on both sides (`serve_one` on the server, `converse` on the
  client) instead of an unbounded `read_line`, so a broken or hostile owner-local
  control client sending data with no `\n` can no longer make a live run's memory
  grow without limit.
- Both interactive terminal-handoff failure paths (a failed foreground-control
  handoff, and the failed post-handoff process-group resume) now emit a
  `container_failed` event — with a new `phase: "foreground"` — before the terminal
  `runner_exit`, so the failure reason reaches the `--jsonl` stream instead of only
  stderr and the "a `container_error` exit is always preceded by `container_failed`"
  invariant holds on these paths too. `foreground` is an additive value in the v1
  `container_failed.phase` enum (no `schema_version` bump).
- `inspect`/`cancel`/`kill` now open the run registry read-only, like `list`/
  `prune` already did, instead of the mutating open `run` uses: a simple query or
  control command against a run no longer creates the registry directory or
  re-asserts its owner-only permissions as a side effect when the directory does
  not yet exist.
- An orphaned registry `.lock` file — one with no paired `.json` record, which
  `Registry::scan` never sees and so never reached `prune` — no longer accumulates
  forever. `Registry::register` now backstops the reservation it makes before
  writing the record: if the write never lands, the freshly created lock file is
  deleted on drop instead of leaked. `prune [--json]` also gained a second pass that
  reaps any orphaned `.lock` file already on disk (e.g. from a hand-edited registry,
  or a `Registration::remove` whose `.json` delete succeeded while its `.lock`
  delete did not), with the same confirm-before-delete safety as the existing
  paired-record reap (a live lock is never touched; an unprobeable one is left in
  place), plus a minimum-age floor so a lock file `Registry::register` has only just
  reserved — created, but not yet locked — is never mistaken for a long-dead orphan
  by a concurrently running `prune`. Its `--json` tally gained an additive
  `orphaned_locks` field alongside the existing `pruned`/`live`/`unprobed`. See
  README.md and [`docs/registry.md`](docs/registry.md), "Reaping — `prune`".

## [0.2.2] - 2026-07-24

### Added
- `run --inherit-stdio` for interactive commands that need the runner's stdin,
  stdout, and stderr handles directly. It preserves an existing terminal while
  retaining containment, JSONL lifecycle events, cleanup, control-plane access,
  and exit-code fidelity; the default closed-stdin plus pipe-and-echo behavior is
  unchanged. The mode is advertised through `probe` and conflicts with capture,
  no-console mode, and the two input-only modes.

### Changed
-

### Fixed
- POSIX inherited-stdio terminal handoff now keeps `SIGTTOU` ignored while the
  interactive child owns the foreground terminal, restores terminal ownership
  first, and then restores the caller's original signal disposition.

## [0.2.1] - 2026-07-23

### Added
- `run --inherit-stdin` and `run --stdin-file <file>` opt-ins. The former shares
  the runner's stdin with the child; the latter streams a readable file through
  ProcessKit and closes stdin at EOF. The modes are mutually exclusive, leave the
  default closed stdin unchanged, and are advertised through probe surface tokens.

### Changed
-

### Fixed
-

## [0.2.0] - 2026-07-23

### Changed

- ci: drop x86_64-apple-darwin from the release/CI target matrix

## [0.1.0] - 2026-07-23

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
- Side-effect-free compatibility probe: `processkit-cli probe --json` is the
  preflight a consumer
  runs on a candidate **before** launching any payload: it prints the binary's
  compatibility surface (package name, version, JSONL `schema_version`, the reserved
  exit-code band, and the CLI surface tokens derived from the live parser) as one
  deterministic JSON line, and spawns no child, opens no registry, and creates no
  container. With `--require-schema-version` / `--require-exit-code-band` /
  `--require-surface` it *verifies* those dimensions and fails closed with the new
  reserved code `PROBE_INCOMPATIBLE` (110) — the next free slot in the `100`–`119`
  band — printing `compatible:false` with concrete `mismatches` rather than a silent
  "ok". The contract is fail-closed across three distinct, parseable outcomes — path
  missing (`NotFound` at spawn), present-but-not-executable (a non-`NotFound` spawn
  error), and present-executable-but-incompatible (exit `110`) — and forbids any
  silent fallback to an uncontained launch. The new code is recorded in
  `docs/exit-codes.md`. Additive only: no existing flag, exit code `100`–`109`, or
  `schema_version: 1` changes meaning.
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
- Machine-readable JSON Schema (draft 2020-12) for the JSONL event contract v1,
  published at `fixtures/schema/v1/schema.json` alongside the golden
  `events.jsonl` fixture: one schema variant per event type plus the shared
  envelope, transcribed from the normative `docs/schema.md`. Adapters
  (`processkit-py`) can validate against it instead of reimplementing the
  shapes by hand. A new test (`tests/events.rs`) validates the golden fixture,
  and several live streams emitted by the through-the-binary tests, against
  the schema, so drift between the schema, the fixture, and the code fails the
  build. `docs/schema.md` remains the normative source of truth on any
  disagreement.
- `list [--json]`: a new subcommand that scans the per-user registry
  (`Registry::entries`) and prints every entry it finds, live and stale alike —
  `run_id`, health, `started_at`, and `endpoint` — the discovery counterpart to
  `inspect`/`cancel`/`kill` for a caller that has lost (or never had) a `run_id`.
  Read-only: it never connects to any runner's control transport, so it has none
  of their unreachable-run failure modes. Without `--json` it prints a
  human-readable table (`no runs registered` for an empty registry); with
  `--json` it prints one JSON object per entry, one per line, sorted by `run_id`
  then `started_at`. An empty registry is not an error (exits `0`), and a single
  corrupt/unreadable record never blinds the command to the healthy entries
  (the same degradation `Registry::entries` already applies). Additive only —
  the new subcommand appears in the `probe` surface tokens automatically.
- `prune [--json]`: a new subcommand that reaps detectably-dead registry entries —
  after a runner dies abruptly its `.json`/`.lock` pair lingers forever, since
  cleanup only runs on an orderly exit. It probes each entry on its own and removes
  only those confirmed stale by a successful liveness probe: a live entry is never
  touched, and an entry whose probe merely fails (its lock file could not be opened
  at all) is left in place rather than assumed dead — deliberately distinct from the
  degradation `Registry::entries` applies for display. Removal reaches files only
  through the scanned record path, never a PID, and holds the stale entry's lock
  while deleting its record and lock file. Without `--json` it reports how many
  entries were reaped, kept live, and left unprobed; with `--json` it prints that
  summary as one JSON object. An empty or already-clean registry is a no-op (exits
  `0`). Additive only — the new subcommand appears in the `probe` surface tokens
  automatically.

### Changed
- Setup/support failures no longer masquerade as an `INTERNAL` (104) runner fault.
  A new reserved code `SETUP` (111) covers a fail-closed setup failure — an async
  runtime that will not build, an unwritable `--jsonl`/`--capture-dir`, or a
  `probe`/`inspect`/control reply that will not serialize — so `INTERNAL` (104) now
  means strictly a genuine invariant violation (a runner bug) and a consumer never
  reads a bad path as one. The `--capture-dir` setup failure's terminal
  `runner_exit` event gains a matching `source: "setup"` (added to the JSONL
  schema); codes `112`–`119` remain reserved.
- The control plane's three clients — `inspect`, `cancel`, and `kill` — all reach a
  live runner over the local transport now; no subcommand returns the runner-range
  "not implemented" code any longer.
- `run` now consumes every flag it parses: `--jsonl` (the JSONL event stream) and
  `--capture-dir` (bounded output capture) are both wired up.
- Internal: the control plane's client-side scaffolding is de-duplicated. The
  `inspect`/`cancel`/`kill` wire exchange (`converse`/`converse_mutation`) is now
  one function generic over the reply type; `inspect_async`/`mutate_async` share a
  single deadline-timeout-to-`unreachable_run` helper; and the three
  current-thread tokio runtime constructions in `run`/`inspect`/`cancel`/`kill`
  now go through one shared builder. No externally visible behavior changes.
- Updated the `processkit` dependency to 2.3.2 (from 2.3.0). `events::abrupt_cleanup_str()`
  now sources the abrupt-owner-death reap scope from `processkit`'s own honest
  capability report (`Command::kill_on_parent_death_scope`, new in 2.3.2) instead of
  reimplementing the per-platform derivation locally; the emitted
  `whole_tree`/`direct_child_only`/`none` wire values are unchanged.

### Fixed
- Unix control sockets now use a short owner-only temporary directory instead of
  inheriting the registry's full path, so deeply nested macOS CI/workspace paths
  cannot exceed `sockaddr_un::sun_path` and silently disable `inspect`.

[Unreleased]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ZelAnton/ProcessKit-CLI/releases/tag/v0.1.0
