# Architecture overview

This document is the map that ties the per-area normative documents together:
the module layout, how one `run` moves data from spawn to exit, how the
control-plane clients (`inspect`/`cancel`/`kill`) reach a live runner, where
this repository's responsibility ends and the `processkit` crate's begins, and
how the test suite is layered. It does not restate the normative contracts
themselves — each links to its own document below — and it is not a substitute
for reading the source; treat it as the entry point for a new contributor,
sketched from the code as of this writing rather than from memory.

## Module map

`processkit-cli` is a single bin crate (`src/main.rs`); every other file under
`src/` is a `mod` of it. Responsibilities, in the order data flows through a
`run`:

| Module | Responsibility |
| --- | --- |
| [`src/cli.rs`](../src/cli.rs) | The CLI-flags half of the compatibility surface: the clap-derived `Cli`/`Command` types for `run`, `inspect`, `cancel`, `kill`, and `probe`. Parsing and shape validation only — each subcommand's behavior lives in its own module. |
| [`src/main.rs`](../src/main.rs) | Entry point. Parses `Cli`, dispatches to the subcommand module, and maps the result onto a process exit code: `run` owns its own exit path (it hard-exits with the child's code and never returns here); every other subcommand's `Result<(), RunnerError>` is mapped through `RunnerError::code()`, and a clap parse failure is mapped onto the runner's own `USAGE` code rather than clap's default. |
| [`src/run.rs`](../src/run.rs) | The `run` subcommand itself: spawns the child into a `processkit::ProcessGroup` this module owns, pipes and echoes its output live, races the child's exit against `--timeout`/`Ctrl-C`/a control-plane command, and drives the shared teardown tiers — the graceful soft stop → grace → hard kill for `timeout`/`Ctrl-C`/`cancel`, and the immediate hard kill (no soft stop, no grace) for a control-plane `kill`. Exit-code fidelity — the child's exact code on a normal completion, a reserved-band code for every runner-imposed ending — is enforced here. |
| [`src/events.rs`](../src/events.rs) | The versioned JSONL lifecycle-event schema and its emitter — this repository's normative, golden-tested public event contract (see [`docs/schema.md`](schema.md)). Also owns argv redaction: the default SHA-256 `argv_sha256` fingerprint and the `HINT_RULES` worker-shape classifier. |
| [`src/capture.rs`](../src/capture.rs) | `--capture-dir` bounded per-stream stdout/stderr capture to files, riding the same tee `run` already echoes through (no second output-reading path). Records, per stream, a full byte counter, a SHA-256 of the bytes written, and independent explicit `truncated`/`write_error` flags, surfaced in the `output_captured` event. |
| [`src/hash.rs`](../src/hash.rs) | The one hand-rolled incremental/one-shot SHA-256 (FIPS 180-4) both `events` (argv fingerprint) and `capture` (streamed transcript hashing) build on, so the project has a single digest primitive and rendering style. |
| [`src/registry.rs`](../src/registry.rs) | The per-user run registry: one record per in-flight run in an owner-only-restricted directory, found by scanning and matching `run_id` (never a PID), with staleness detected via an OS advisory lock the live runner holds (see [`docs/registry.md`](registry.md)). The first brick of the control plane. |
| [`src/control.rs`](../src/control.rs) | The live-run control plane: the per-run local IPC transport (unix domain socket / Windows named pipe, owner-restricted) stood up inside `run`, its line-oriented `inspect`/`cancel`/`kill` wire protocol, and the three clients that speak it (see [`docs/control-plane.md`](control-plane.md)). |
| [`src/probe.rs`](../src/probe.rs) | The side-effect-free `probe` subcommand: reports (and, with `--require-*`, verifies) this binary's version/`schema_version`/exit-code band/CLI surface as one JSON line. |
| [`src/exit.rs`](../src/exit.rs) | The reserved runner-own exit-code band (`100`–`119`) constants — the exit-code half of the compatibility surface (see [`docs/exit-codes.md`](exit-codes.md)). |

## Data flow of one `run`

A `processkit-cli run` moves through the same sequence on every platform,
implemented in `run::execute`/`run::run_async` (`src/run.rs`):

1. **Spawn.** The child is built from `processkit::Command` and spawned into a
   `ProcessGroup` this process owns — not a shared or global one — so the
   group's kernel-backed kill-on-drop (Windows Job Object, Linux
   cgroup/POSIX-group, macOS process group) reaps the whole tree on every exit
   path *this process lives to observe* (normal completion, timeout, `Ctrl-C`,
   control-plane `cancel`/`kill`). That guarantee does not extend to this
   process's own **abrupt** death (crash/`SIGKILL`/`TerminateProcess`), which
   skips `Drop` entirely: reaping a leaked grandchild after the runner itself
   dies abruptly is a platform-derived tri-state — `whole_tree` on Windows (Job
   Object survives the owner's abrupt death), `direct_child_only` on Linux
   (`kill_on_parent_death`/`PR_SET_PDEATHSIG`, direct child only), `none` on
   macOS/BSD — surfaced per run as `run_started`'s `abrupt_cleanup` field (see
   K-005). A `run_started` event (run id, root PID, containment mechanism,
   abrupt-cleanup tri-state, working directory) opens the JSONL stream.
2. **Pump events.** `processkit`'s line pump concurrently reads the child's
   stdout/stderr and drives two things off the same read: the live echo to
   this process's own stdout/stderr (`src/run.rs`), and — when
   `--capture-dir` is set — the per-stream tee in `src/capture.rs`. A
   `members_snapshot` event records the container's PID-only member list.
3. **Capture and hash.** `src/capture.rs`'s `CaptureTee` mirrors every echoed
   byte into a bounded capture file per stream, hashing what actually reached
   disk with `src/hash.rs`'s incremental SHA-256 and recording an explicit
   ceiling-truncation flag and write-error flag — never inferred from the
   file's size. This stage is a no-op, with no capture files and no
   `output_captured` event, unless `--capture-dir` was passed.
4. **Teardown.** Runner-imposed endings split into two tiers, not one shared
   path. `--timeout` elapsing, interactive `Ctrl-C`, and a control-plane
   `cancel` reaching the live runner over `src/control.rs` all drive the
   *same* graceful path: a soft stop (`SIGTERM` to the tree on Unix; no
   soft-signal tier on Windows yet, so the grace window still elapses honestly
   with no signal sent), a `--grace` wait, then the owning `ProcessGroup`'s
   kernel-backed hard kill-on-drop. A control-plane **`kill`** is not part of
   that tier: it skips the soft stop and the grace window entirely and
   hard-kills the whole tree immediately via the same kill-on-drop mechanism —
   documented as immediate on purpose, not a shorter grace. A normal
   completion instead reaps via the same drop once the child's own exit is
   observed (`root_exited`, then `cleanup_started`/`cleanup_finished`).
5. **`runner_exit`.** The terminal JSONL event closes the stream, always
   carrying the outcome — the child's own exit code on a normal completion,
   or the reserved code for whichever runner-imposed ending fired
   (`timeout`/`cancelled`/`killed`, or a control-plane `cancelled`/`killed`).
   The process's own exit code (see [`docs/exit-codes.md`](exit-codes.md))
   mirrors that same outcome, so a shell that never reads the JSONL stream
   still gets a faithful, distinguishable signal.

## Control-plane contour

`inspect`, `cancel`, and `kill` (`src/control.rs`) never address a live `run`
by PID; they resolve it through the run registry (`src/registry.rs`):

1. **Registry scan.** `registry::Registry::entries` lists every record in the
   per-user registry directory and classifies each as live or
   [`registry::Health::Stale`] by probing the record's advisory liveness
   lock — a dead runner's leftover record is detected this way, not by mere
   file existence (see [`docs/registry.md`](registry.md)).
2. **Endpoint resolution.** `control::resolve_live_endpoint` matches the
   requested `run_id` against the live entries only. More than one live match
   is an **ambiguous run id** — a hard `CONTROL` (103) failure for every verb,
   never a guess at which entry the scan happened to return first. The
   mutating verbs (`cancel`/`kill`) additionally re-run this resolution
   immediately before writing the verb (`mutate_async`), narrowing — though
   not fully closing — the TOCTOU window against a duplicate registering
   mid-flight (see [`docs/control-plane.md`](control-plane.md), "Ambiguous
   run id").
3. **Verb over transport.** The client connects to the resolved endpoint — a
   unix domain socket or a Windows named pipe, both owner-restricted — and
   speaks the shared line-oriented wire protocol: one request-verb line out,
   one JSON reply line in. `inspect` is read-only and prints a `Snapshot`;
   `cancel`/`kill` are mutating and reuse `run`'s own teardown tiers exactly as
   described above — `cancel` the graceful soft-stop → grace → hard-kill tier,
   `kill` the immediate hard-kill tier — replying with a `ControlAck` before
   the run ends.

See [`docs/registry.md`](registry.md) for the registry's location, record
format, and staleness signal, and [`docs/control-plane.md`](control-plane.md)
for the full wire protocol and both dead-runner cases (stale entry vs. died
mid-conversation).

## Boundary with `processkit`

`processkit-cli` is a thin, standalone wrapper: the [`processkit`
crate](https://crates.io/crates/processkit) is the single source of truth for
containment, teardown, PID-reuse discipline, and process-tree lifecycle
semantics, and this repository builds strictly on its public API rather than
reimplementing any of that — a genuine gap becomes an additive request in
ProcessKit-rs's own backlog, never a local fork of the semantics (this is a
settled, repository-wide decision the module docstrings cite verbatim).
Concretely:

- **What `processkit` owns:** the kernel-backed container
  (`ProcessGroup`) and its kill-on-drop teardown, the async child-output line
  pump (with a byte-capped `OutputBufferPolicy`), and the environment
  builder (`Command::env`/`env_remove`/`env_clear`) `run`'s `--env*` flags map
  onto directly.
- **What this runner owns:** the CLI surface (`src/cli.rs`), the versioned
  JSONL event contract and argv redaction (`src/events.rs`), the reserved
  runner-own exit-code band (`src/exit.rs`), bounded diagnostic capture with
  hashing (`src/capture.rs`, `src/hash.rs`), the per-user run registry
  (`src/registry.rs`), and the live-run control plane
  (`src/control.rs`)/preflight probe (`src/probe.rs`) built on top of it —
  none of which `processkit` itself provides.

`README.md`'s introduction states the same division for a project outsider:
`processkit-cli` "runs one program inside ProcessKit's kernel-backed
containment boundary and reports the run lifecycle," while "ProcessKit-rs
remains the sole owner of containment, teardown, PID-reuse discipline, and
lifecycle semantics." Out of scope entirely: IPC-to-child protocols beyond the
control plane above, scheduling/pooling/retries beyond what `processkit::Command`
offers, a shell mode, and PTY support (deferred in the core crate).

## Test tiers

Three tiers, increasing in weight and decreasing in how often they run:

- **Unit.** Each `src/*.rs` module carries its own `#[cfg(test)] mod tests`
  (for example the SHA-256 vector tests in `src/hash.rs`, or the
  `ProcessGroup`/`Emitter`-driven helper tests in `src/run.rs`) exercised by a
  plain `cargo test --bin processkit-cli` (this crate is bin-only — there is
  no `--lib` target). Internal helpers are tested against real
  `processkit`/`Emitter` objects, never a mock layer.
- **Integration.** `tests/` drives the *built binary*
  (`env!("CARGO_BIN_EXE_processkit-cli")`), not the library, because the value
  this crate adds over ProcessKit-rs's own suite is the binary plus its
  contracts: `tests/run.rs`, `tests/events.rs`, `tests/registry.rs`,
  `tests/probe.rs`, and `tests/integration.rs` cover through-the-binary
  scenarios, sharing fixtures/helpers from `tests/common/mod.rs`. This is the
  default `cargo test` tier.
- **End-to-end (`e2e`, feature-gated).** `tests/e2e.rs` is heavier still: it
  spawns real multi-level process trees, observes liveness from *outside* the
  runner (an OS process-table probe, not the container's own member list),
  and stresses concurrent runs, nested Windows Job Objects, PID-reuse storms,
  and abrupt runner death. It is gated behind the `e2e` Cargo feature (with
  its `src/bin/e2e_helper.rs` worker binary) so it stays off in the default
  `cargo test` and runs explicitly via
  `cargo test --features e2e --test e2e -- --nocapture`; CI runs it as a
  separate job. See `CONTRIBUTING.md`, "End-to-end tests".

## Normative documents

Each area of the compatibility surface has its own normative document; this
overview only sketches how they connect:

- [`docs/registry.md`](registry.md) — the per-user run registry: location,
  record format, staleness signal.
- [`docs/control-plane.md`](control-plane.md) — the local transport, the wire
  protocol, and the `inspect`/`cancel`/`kill` clients.
- [`docs/exit-codes.md`](exit-codes.md) — the reserved runner-own exit-code
  band and the child-fidelity rule.
- [`docs/schema.md`](schema.md) — the versioned JSONL lifecycle-event schema.
- [`docs/ROADMAP.md`](ROADMAP.md) — the delivery status and the remaining
  ProcessKit-rs dependencies (this document describes the implementation).
