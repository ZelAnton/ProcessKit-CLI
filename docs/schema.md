# JSONL event schema (v1)

This is the **normative description** of processkit-cli's JSONL lifecycle-event
contract. It is part of the project's public compatibility surface — *CLI flags +
exit-code ranges + `schema_version`* (see `AGENTS.md`) — because adapters, in
particular the processkit-py CLI, pin `schema_version` and reimplement these
shapes. Treat every field below as public API.

- The in-code source of truth is `src/events.rs`.
- The golden sample stream is
  [`fixtures/schema/v1/events.jsonl`](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/fixtures/schema/v1/events.jsonl); the
  golden test (`events::tests::golden_stream_matches_the_fixture`) keeps this
  document, the code, and the fixture in lockstep.
- A machine-readable JSON Schema (draft 2020-12) is published at
  [`fixtures/schema/v1/schema.json`](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/fixtures/schema/v1/schema.json) — one
  variant per event type plus the shared envelope, transcribed from this
  document. **This prose document remains the normative source of truth**; the
  JSON Schema is a mechanical mirror of it, kept honest by
  `tests::golden_fixture_validates_against_the_schema` (`tests/events.rs`),
  which validates the golden fixture (and, in several other tests in that
  file, live streams emitted by the binary) against it — so a discrepancy
  between the schema and the fixture/code fails CI rather than drifting
  silently. On any disagreement between the schema and this document, trust
  this document and treat the schema as needing a fix. The schema's version is
  synchronized with `schema_version`: it lives under `fixtures/schema/v1/`
  alongside the fixture, and a breaking change that bumps `schema_version`
  (see "Versioning") moves both to a new `fixtures/schema/vN/` directory
  together, never one without the other.

### Getting the schema without a git checkout

`fixtures/schema/v1/schema.json` lives in the repository, so a consumer who
only has an installed binary (`cargo install`) or an unpacked release
archive — no clone, no tag to check out — could otherwise only get the exact
schema for their version by guessing which git tag matches. Two offline
alternatives avoid that:

- `processkit-cli probe --json --print-schema` prints the schema document
  embedded into that specific binary at build time (`src/probe.rs`'s
  `SCHEMA_JSON`, via `include_str!` — the file above stays the single source
  of truth, this is a verbatim, byte-for-byte copy of it, never a
  hand-maintained second one). It replaces the usual probe report, and
  **cannot be combined with any `--require-*` flag**: that combination is
  rejected at parse time as an ordinary usage error (exit `100`), not silently
  accepted with the requested checks skipped — a `probe` invocation that asks
  for expectations to be verified must never exit `0` without verifying them
  (see `docs/integration.md`, "Fail-closed preflight: `probe`"). `--print-schema`
  itself is an ordinary, additive CLI surface token
  (`probe:--print-schema`) like any other `probe` flag.
- Every release archive (`.github/workflows/release.yml`) bundles a `schema/`
  directory alongside the binary, `completions/`, and `man/`, containing
  `schema.json` and the golden `events.jsonl` fixture exactly as checked out
  for that release.

And to *check* a stream against it without a checkout — or a JSON Schema
validator of your own — `processkit-cli events --file <stream.jsonl> --validate`
validates every line against that same embedded document, reporting each
violation by line number and exiting `EVENTS_INVALID` (`114`) when any line does
not conform (see [`docs/exit-codes.md`](exit-codes.md), "Checking a stream:
`events --validate`"). It is the recommended way for an adapter to keep its own
recorded fixtures honest against the runner version it targets. The checker is
in-binary and adds no runtime dependency: it interprets the embedded document
over the keyword subset that document uses and **refuses to run** on anything it
does not implement, and the test tier holds its verdict against a real JSON
Schema engine's, line for line, over the golden fixture and a generated mutation
corpus.

## Transport

- Events are written to the file named by `run`'s `--jsonl` option, **never to
  stdout**. The child's stdout and stderr pass through untouched; runner
  diagnostics go to the `--jsonl` file or to stderr, never interleaved into the
  child's stdout (`AGENTS.md`, "Streams are strictly separated").
- The file is **one event per line** (JSON Lines): each line is a single,
  complete JSON object followed by `\n`. Lines are UTF-8.
- The `--jsonl` file is **created or truncated** at the start of a run, so it
  holds exactly that run's stream. Each line is flushed as it is written, so the
  stream is durable even though a completed run forwards the child's exit code via
  an immediate process exit.
- If the `--jsonl` file cannot be created, the run **fails closed** before the
  child is spawned (a runner-band exit code) rather than running the child with no
  event stream. A write failure *after* the child has started is best-effort: the
  runner warns once on stderr and continues, because the child's exit-code
  fidelity outranks diagnostics.

## Envelope

Every line shares a common envelope, always in this order:

| Field            | Type    | Notes                                                                 |
|------------------|---------|-----------------------------------------------------------------------|
| `schema_version` | integer | Always `1` for this version. See "Versioning".                        |
| `time`           | string  | Emission time, RFC 3339 UTC, millisecond precision (`…Z`).            |
| `event`          | string  | The event type tag (snake_case); selects the remaining fields below.  |

The `time` field is the moment the runner emitted the event. For `root_exited` it
therefore doubles as the child's exit timestamp — the moment the runner observed
the child leave.

## Events

Fields marked *nullable* are always present; when a value is unknown or does not
apply they are the JSON literal `null` (explicitly absent), never omitted.

### `run_started`

The run has begun: the child is spawned into the container.

| Field            | Type              | Notes                                                                 |
|------------------|-------------------|-----------------------------------------------------------------------|
| `run_id`         | string            | The `--run-id` value, or a generated `run-<pid>-<unix_nanos>`.         |
| `labels`         | object            | Operator labels from `run --label`; empty when none were supplied. Keys are sorted for deterministic JSON. |
| `root_pid`       | integer, nullable | The root child's PID; `null` if the backend exposed none.             |
| `mechanism`      | string            | Containment mechanism: `job_object`, `cgroup_v2`, or `process_group`. |
| `abrupt_cleanup` | string            | Cleanup surviving abrupt runner death: `whole_tree`, `direct_child_only`, or `none`. |
| `cwd`            | string, nullable  | The child's absolute working directory; `null` if it could not be resolved. |
| `command`        | object            | The command, redacted by default — see "Command redaction".           |

`abrupt_cleanup` is distinct from `mechanism` and from ordinary teardown. It is
`whole_tree` on Windows because closing the runner's last Job Object handle kills
all members; `direct_child_only` on Linux because the runner enables ProcessKit's
parent-death signal for the root child while cgroups themselves persist; and
`none` on macOS/other Unix because the current public API has no parent-death
primitive there. Normal completion, timeout, and cancellation still invoke the
reported container mechanism's regular teardown.

### `members_snapshot`

A point-in-time snapshot of the container's members. It is a snapshot, not a
census: a listed PID may exit immediately afterward, and a process spawned during
the read may be missing.

| Field        | Type            | Notes                              |
|--------------|-----------------|------------------------------------|
| `reason`     | string          | What asked for this snapshot: `spawn` or `interval` (below). |
| `read_error` | boolean         | `true` when the member read itself failed; see "Honest degradation on a failed sample" below. |
| `members`    | array of member | Each entry is a *member* (below).  |

A **member** object:

| Field        | Type              | Notes                                                    |
|--------------|-------------------|----------------------------------------------------------|
| `pid`        | integer           | The process id.                                          |
| `ppid`       | integer, nullable | Parent pid — see "Enriched member fields".               |
| `name`       | string, nullable  | Executable name — see "Enriched member fields".          |
| `start_time` | string, nullable  | Opaque, platform-specific start-time token, as a decimal string — see "Enriched member fields". |

**How many, and when (`reason`).** Every run emits one snapshot immediately after
`run_started`, carrying `reason: "spawn"`. A run given `run --snapshot-interval
<duration>` **additionally** re-emits the same event on that cadence for as long
as the child runs, each carrying `reason: "interval"` — recorded observability for
how the tree evolved (when the worker fleet grew, whether helpers lingered, what
the tree looked like just before a deadline fired), for the long, quiet, or
detached runs where nobody is likely to have been watching live with `inspect` at
the interesting moment.

- `reason` and `read_error` are **always present**, including on the single
  `spawn` snapshot a run without the flag emits. `reason` is the only difference
  between a post-spawn snapshot and a periodic one: both are produced by the same
  code through the same `members_info()` enrichment, so a periodic snapshot's
  `members` cannot drift in shape from the first one's.
- The periodic snapshots stop the moment the run's ending is decided, so they
  appear **only** between `run_started` and the ending's own event
  (`root_exited`, or the `timeout`/`cancelled`/`killed` reason event) — never
  interleaved into the `cleanup_started`/`cleanup_finished` teardown pair. See
  "Ordering".
- Repeating an existing event is **additive within schema v1** — no field is
  renamed, retyped, or given a new meaning, and a reader that pins a version must
  already tolerate additional events (see "Versioning"). The cadence is also
  opt-in: without `--snapshot-interval` a run emits exactly the one snapshot, at
  exactly the point, it always did. The event's *wire form*, however, changed for
  every run including the flagless one: `reason` and `read_error` are new,
  always-present fields, additive within v1 but not "unchanged" (see
  [Compatibility and upgrades](compatibility.md#schema-pinning) for the reader
  obligations that follow).
- Snapshots are read from the container's own member list, not from the runner's
  output pump, so `--snapshot-interval` composes with every I/O mode —
  `--inherit-stdio` included, unlike `--idle-timeout`.

**Honest degradation on a failed sample.** When the `members_info()` read itself
fails, the snapshot is **still emitted**, with `read_error: true` and an empty
`members` array — the same convention `cleanup_started`/`cleanup_finished` use for
their own failed reads (see "Honest degradation on a teardown read failure").
`members` is then a fallback, not an observation: a consumer must check
`read_error` before reading an empty array as a confirmed-empty tree. The runner
also warns on its stderr, but **that warning is not the contract**: a `--detach`ed
runner's stdin/stdout/stderr are `null` (see
[Detached runs](detached-runs.md#output-behavior)), so in the long, quiet, detached
runs this cadence exists for, the flagged event in the JSONL file is the only
report of the failure that reaches anyone. A failed sample therefore never appears
as a gap in the cadence; a genuine gap means the run ended, the interval had not
elapsed yet, or the stream itself stopped (see "Stream size" below).

**Stream size.** The cadence is the first thing in this stream whose event count
scales with a run's *duration*, and it is deliberately unbounded — see
[Running commands](running-commands.md#recorded-tree-snapshots) for the recorded
decision, the sizing arithmetic, and how to choose an interval.

### `root_exited`

The root child exited on its own.

| Field     | Type              | Notes                                                          |
|-----------|-------------------|----------------------------------------------------------------|
| `outcome` | string            | `exited`, `signalled`, `timed_out`, or `unknown`.              |
| `code`    | integer, nullable | The exit code for `exited`; `null` otherwise.                  |
| `signal`  | integer, nullable | The signal number for a Unix `signalled` death; `null` otherwise. |

On Windows a killed process reports `exited` with a platform code (there is no
signal abstraction), so `signalled` is Unix-only.

### `cleanup_started`

Container teardown is beginning.

| Field            | Type    | Notes                                          |
|------------------|---------|------------------------------------------------|
| `members_before` | integer | The tree size (member count) about to be reaped. |
| `read_error`     | boolean | `true` when the pre-cleanup member read itself failed; see "Honest degradation on a teardown read failure" below. |

### `cleanup_finished`

Container teardown finished (after the hard kill).

| Field            | Type              | Notes                                                                   |
|------------------|-------------------|-------------------------------------------------------------------------|
| `remaining`      | integer           | Count of `remaining_pids`.                                              |
| `remaining_pids` | array of integer  | Post-kill member snapshot; normally empty.                             |
| `soft_terminate` | string, nullable  | The soft-stop tier for a runner-imposed ending (below); `null` on the natural-exit path. |
| `shutdown`       | object, nullable  | Pre-attempt capability plus ProcessKit `ShutdownReport` observations; `null` when no soft stop was attempted. |
| `read_error`     | boolean | `true` when the post-kill member read itself failed; see "Honest degradation on a teardown read failure" below. |

`remaining_pids` is a snapshot: on the Job Object and cgroup mechanisms a process
leaves membership on exit, so it is empty after the kill; on the POSIX
process-group fallback an unreaped just-exited child can still be listed until it
is reaped. `soft_terminate` is one of:

- `signalled` — a soft stop really was delivered to the tree: on Unix a `SIGTERM`
  broadcast; on Windows, where a Job Object has no POSIX signal, ProcessKit's
  best-effort soft *close* — a `WM_CLOSE` to every top-level window owned by a live
  member plus a console `CTRL_BREAK` to a child opted in with
  `--windows-graceful-ctrl-break`.
- `unsupported` — nothing in the tree could receive a soft stop, so none was sent
  and the runner does not pretend otherwise. Windows-only in practice, and the
  ordinary case there for a plain console child: no windowed member and no
  console-CTRL leader means the soft tier has nothing to trigger. The grace window
  still elapsed before the atomic Job Object kill.
- `failed` — the soft stop could not be delivered; the hard kill ran regardless.

`shutdown` is `null` for natural exits and immediate `kill` endings. Otherwise it
contains:

| Field | Type | Notes |
|---|---|---|
| `soft_stop_scope` | string | Pre-attempt capability: `whole_tree`, `opt_in_members`, or `none`. |
| `soft_signal` | string | Observed delivery: `sent`, `unsupported`, or `failed`. |
| `members_before` / `members_after` | integer, nullable | ProcessKit's point-in-time counts; `null` on read failure. |
| `drained_within_grace` | boolean, nullable | Whether every member exited before escalation. |
| `escalated` | boolean, nullable | Whether ProcessKit hard-killed survivors. |
| `elapsed_ms` | integer, nullable | Actual stop-driver duration in milliseconds. |

The last three fields are `null` only when `ProcessGroup::stop` itself failed and
could not return a `ShutdownReport`; the owning container's hard-kill backstop still
runs. This is additive to schema v1 and leaves `soft_terminate` in place for existing
consumers.

**Honest degradation on a teardown read failure.** `members_before`/`remaining`/
`remaining_pids` are read from the live container (`ProcessGroup::members()`),
which can itself fail (an OS-level enumeration error). Rather than let a read
failure masquerade as a confirmed `0`/empty observation, each event carries an
explicit `read_error` flag: `false` on every successful read (the common case,
unaffected by this), `true` when the read failed, in which case the numeric
field(s) fall back to `0`/an empty array — not a fabricated fact, only the
absence of one. A consumer that treats `cleanup_finished.remaining == 0` as
"the tree is confirmed empty" must first check `read_error` is `false`; the
runner also warns on stderr whenever this happens, though — as for
`members_snapshot`'s own flag above — that warning reaches nobody in a detached
run, so the flag in the stream, not the warning, is the contract. This mirrors
`output_captured`'s `write_error` flag (both are explicit failure markers, never
inferred from the accompanying data alone).

### `limit_hit`

A requested ProcessKit resource limit (`--max-memory`, `--max-processes`, or
`--cpu-quota`) could not be applied.

| Field    | Type             | Notes                                            |
|----------|------------------|--------------------------------------------------|
| `limit`  | string           | Which limit could not be applied: `memory`, `processes`, or `cpu`. |
| `detail` | string, nullable | Human-readable detail; `null` if none.           |

**When it is emitted.** Enforcement of a whole-tree cap needs a real container — a
Windows Job Object or a Linux cgroup v2. Where none can carry the request, the run
fails **fast** rather than running silently unbounded, and this event records it.
The emission is deliberately narrow:

- **Only the "could not be applied" branch.** ProcessKit signals a limit failure
  only at group creation (pre-spawn — the child never started); it exposes no
  separate "the tree was killed mid-run for exceeding a cap" runtime signal (the OS
  reaps an offender itself — a Job Object/cgroup OOM or CPU throttle — without the
  crate translating that into an event). So `limit_hit` covers the *unenforceable /
  unsupported* case, not a live overrun: `memory`/`cpu`/`processes` where the
  platform has no whole-tree container at all (macOS/the BSDs, the Linux
  process-group fallback), or a Linux cgroup v2 whose controllers can't be enabled
  (not the real hierarchy root — under systemd, an ordinary container, or typical
  CI; see `README.md`, "Resource limits"). This is the pre-spawn admission
  failure only; post-run evidence for successfully applied caps is carried by
  the additive `limit_evidence` event below and follows the platform matrix in
  [`docs/resource-limits.md`](resource-limits.md#applied-limit-versus-observed-limit-hit).
- **Nonsense values never reach it.** A degenerate value (`--max-memory 0`, a
  non-positive/non-finite `--cpu-quota`) is a `USAGE` (100) form error rejected at
  argument-parse time, so `limit_hit` never carries an "invalid value" reason.
- **Ordering.** `limit_hit` is emitted **first**, then the same
  `container_failed` (`phase: "create"`) → `runner_exit` (`source:
  "container_error"`, code `BACKEND` = 102) tail every other group-creation failure
  takes. The dedicated `limit_hit` event — not the exit code — is the authoritative
  signal that the ending was a resource limit (`docs/exit-codes.md`, "Why a band is
  not enough on its own"). A run whose caps *were* applied emits no `limit_hit` at
  all and proceeds normally. Its post-run evidence, when requested, is the
  separate `limit_evidence` event below.

### `limit_evidence`

Post-run, per-axis evidence for a run that requested at least one resource cap
and whose `ProcessGroup::with_options` call successfully created the container.
The runner reads `ProcessGroup::limit_evidence()` while that group still exists,
after the ending event (`root_exited`, `timeout`, `cancelled`, `killed`, or
`output_overflow`) when one exists, and before `cleanup_started` consumes the
group. If group creation returns ProcessKit's `ResourceLimit` error, the group
does not exist: the runner emits the pre-spawn `limit_hit` event and its existing
backend-error tail, with no `limit_evidence` event.

| Field | Type | Notes |
|---|---|---|
| `memory` | string | `tripped`, `not_tripped`, or `unknown`. |
| `processes` | string | `tripped`, `not_tripped`, or `unknown`. |
| `cpu` | string | `tripped`, `not_tripped`, or `unknown`. |

`tripped` is emitted only for authoritative kernel evidence that the cap
engaged; `not_tripped` is authoritative evidence that it did not engage (and is
also ProcessKit's result for an uncapped axis); `unknown` means a successfully
created group's mechanism cannot provide a post-run answer. Linux cgroup v2 can
report all three states. A successfully created Windows Job Object reports
`unknown` for capped axes. POSIX process-group fallback and macOS/BSD process
groups fail before a capped group exists, so they emit `limit_hit` and no
`limit_evidence`. The event is absent when no cap was requested. This is an
additive event within `schema_version = 1`.

### `timeout`

A runner deadline elapsed while the child was still running: either the whole-run
`--timeout`, or the `--idle-timeout` (the child produced no output for the idle
window). Both share this event, the reserved `TIMEOUT` (106) terminal code, and the
soft-stop → grace → hard-kill teardown described by the following `cleanup_started` /
`cleanup_finished` events (see `docs/exit-codes.md`); the always-present `reason`
field tells them apart.

| Field        | Type              | Notes                                     |
|--------------|-------------------|-------------------------------------------|
| `timeout_ms` | integer           | The deadline that elapsed, milliseconds — the whole-run window for `overall`, the idle window for `idle`. |
| `grace_ms`   | integer, nullable | The `--grace` window, ms; `null` if unset. |
| `reason`     | string            | Which deadline fired: `overall` (`--timeout`) or `idle` (`--idle-timeout`). |

`--idle-timeout` re-arms its deadline on every chunk of the child's output, so a
child that keeps producing output is never reaped no matter how long it runs — only
one that goes silent past the window is. It reuses `TIMEOUT` (106) rather than
minting a new exit code (the same class of ending — a deadline the runner enforced —
distinguished by the more specific `reason` on this earlier event) and its terminal
`runner_exit` `source` stays `timeout`, exactly as for `--timeout`. It requires the
runner's output pump, so it conflicts with `--inherit-stdio` at parse time (like
`--capture-dir`); it does compose with `--capture-dir`, whose tee re-arms the same
one timer.

### `output_overflow`

A capture stream exceeded its configured per-stream ceiling while
`--capture-overflow cancel` was active. The event is emitted before the same
soft-stop → grace → hard-kill teardown used by a timeout.

| Field       | Type              | Notes |
|-------------|-------------------|-------|
| `stream`    | string            | `stdout` or `stderr`, whichever crossed the ceiling first. |
| `max_bytes` | integer           | The active per-stream `--capture-max-bytes` ceiling (or its 8 MiB default). |
| `grace_ms`  | integer, nullable | The `--grace` window, ms; `null` if unset. |

The terminal `runner_exit` has `source: "output_overflow"`, code
`OUTPUT_OVERFLOW` (113), and `child_code: null`. `output_captured` still follows
cleanup and reports the transcript's final counters and truncation flags. Without
the opt-in `cancel` policy, crossing the same ceiling emits no `output_overflow` and
continues the run, preserving the default truncate-only behavior.

### `cancelled`

The run was cancelled and torn down through the shared soft-stop → grace → hard-kill
path. `source` names the trigger, and the terminal `runner_exit` carries the matching
reserved code:

- `ctrl_c` — a local interactive `Ctrl-C`; terminal code `CANCELLED` (107).
- `sigterm` — **Unix only**: the runner received `SIGTERM`, the standard external stop
  (`kill <pid>`, `systemctl stop`, a cancelled CI job, a supervisor's shutdown
  timeout); terminal code `CANCELLED` (107).
- `sighup` — **Unix only**: the runner received `SIGHUP` — its controlling terminal
  went away (a closed terminal, a dropped SSH session); terminal code `CANCELLED`
  (107).
- `ctrl_break` — **Windows only**: the runner caught `CTRL_BREAK_EVENT`; terminal
  code `CANCELLED` (107).
- `ctrl_close` — **Windows only**: the runner caught `CTRL_CLOSE_EVENT` (the console
  window is being closed); terminal code `CANCELLED` (107). Windows gives the
  handler only a short window (about 5 seconds) before terminating the process
  regardless — see "Timeouts, cancel, and grace" in `README.md` for how the
  effective `--grace` is bounded so this event's own teardown can fit inside it.
- `ctrl_logoff` — **Windows only**: the runner caught `CTRL_LOGOFF_EVENT` (the user
  is logging off); terminal code `CANCELLED` (107).
- `ctrl_shutdown` — **Windows only**: the runner caught `CTRL_SHUTDOWN_EVENT` (the
  system is shutting down); terminal code `CANCELLED` (107).
- `control_cancel` — a `cancel` command that reached the live runner over its control
  plane (see [`docs/control-plane.md`](control-plane.md)); terminal code
  `CONTROL_CANCELLED` (108).

They all share this event because they share the teardown; the `source` and the
terminal code tell them apart. The local signals/events deliberately share the one
`CANCELLED` code — they are the same class of ending (a local signal stopped the run) —
so a consumer that needs to know *which* one arrived reads this `source`, one event
before the terminal `runner_exit`. The Unix signal and Windows console-control-event
values are **additive**: a run that is never signalled emits exactly the stream it did
before, and a consumer that only knows `ctrl_c`/`control_cancel` still sees a
well-formed `cancelled` event with the same fields.

Catching `SIGTERM`/`SIGHUP` (Unix) and `CTRL_BREAK`/`CTRL_CLOSE`/`CTRL_LOGOFF`/
`CTRL_SHUTDOWN` (Windows) is what makes this teardown happen at all on those paths:
their default disposition terminates the runner outright, which would skip the
`cancelled` / `cleanup_started` / `cleanup_finished` / `runner_exit` events and leave the
run's registry entry behind stale until `prune` — the ending would go unreported to
any observer of the event stream or registry, even though the tree itself is not left
orphaned on every platform: the abrupt-owner-death reap covers only the direct child
on Linux, nothing at all on macOS/BSD, and the *whole* tree on Windows (closing the
runner's last Job Object handle; see `cleanup_finished` and `docs/registry.md`).
One exception, deliberate: a Unix signal whose disposition is already `SIG_IGN` when
the runner starts (what `nohup` does to `SIGHUP`) is left ignored rather than
un-ignored behind the operator's back, so no `cancelled` event is produced for it —
and none is owed, since an ignored signal would not have ended the run either.

**Known limitation, Windows only:** a *repeat* Unix signal arriving mid-teardown is
silently absorbed (the OS keeps the disposition installed for the process's whole
lifetime, independent of listener state), but a *second* Windows console-control
event arriving after teardown has already begun is not — it falls through to the
OS's default handling and terminates the runner outright, before the terminal
events above are written. See `README.md`, "Timeouts, cancel, and grace", and the
`#[cfg(windows)]` arm of `wait_for_cancel_signal` in `src/run/signals.rs` for the full
reasoning behind this accepted trade-off.

| Field      | Type              | Notes                                                        |
|------------|-------------------|--------------------------------------------------------------|
| `source`   | string            | `ctrl_c`, `sigterm` (Unix), `sighup` (Unix), `ctrl_break` (Windows), `ctrl_close` (Windows), `ctrl_logoff` (Windows), `ctrl_shutdown` (Windows), or `control_cancel`. |
| `grace_ms` | integer, nullable | The **effective** `--grace` window, ms; `null` if unset. For a Windows `ctrl_close` this may be less than the requested `--grace` (capped to fit the OS's own termination window — see `README.md`, "Timeouts, cancel, and grace"); every other trigger echoes the request unchanged. |

### `killed`

The run was killed by a control-plane `kill` command: an **immediate** hard kill of
the whole tree, with no soft stop and no grace (unlike `cancelled`, which waits out
the grace window first). The teardown it triggers is described by the following
`cleanup_started` / `cleanup_finished` events — where `soft_terminate` is `null`,
because no soft stop was attempted — and the run's terminal code is the reserved
`CONTROL_KILLED` (109). See [`docs/control-plane.md`](control-plane.md).

| Field    | Type   | Notes           |
|----------|--------|-----------------|
| `source` | string | `control_kill`. |

### `spawn_failed`

The program could not be started (not found, not executable, bad `--cwd`): the
child never ran.

| Field     | Type    | Notes                                          |
|-----------|---------|------------------------------------------------|
| `code`    | integer | The runner-band exit code (`SPAWN`, 101).      |
| `message` | string  | Human-readable failure reason.                 |

### `container_failed`

Creating the container, joining the child to it, or handing the terminal to an
interactive child failed.

| Field     | Type    | Notes                                                                 |
|-----------|---------|-----------------------------------------------------------------------|
| `phase`   | string  | `create` (the container could not be created), `attach` (the launch into it failed), or `foreground` (handing the terminal to an interactive child failed after it had already spawned). |
| `code`    | integer | The runner-band exit code (`BACKEND`, 102).                           |
| `message` | string  | Human-readable failure reason.                                        |

### `runner_exit`

The **terminal event of every run**: the exact code the runner process returns. It
is always emitted, including on the runner's own failure, so a child's exit code
is never lost or aliased even when the process returns a runner-band code
(`AGENTS.md`, "Exit-code fidelity"; `docs/exit-codes.md`).

| Field        | Type              | Notes                                                                       |
|--------------|-------------------|-----------------------------------------------------------------------------|
| `code`       | integer           | The exit code the runner process returns (child's code, or a runner-band code). |
| `source`     | string            | Why the runner exited: `child_exit`, `timeout`, `output_overflow`, `cancelled`, `control_cancel`, `control_kill`, `spawn_error`, `container_error`, `internal`, or `setup`. |
| `child_code` | integer, nullable | The child's own exit code when it exited on its own; `null` for a runner-imposed ending or a child that never produced one. |

When `source` is `child_exit`, `code` equals `child_code`. For a runner-imposed
ending (`timeout` / `cancelled` / `control_cancel` / `control_kill`) or a pre-run
failure (`spawn_error` / `container_error` / `setup`), `child_code` is `null` and
`code` is the runner-band value. `setup` names a fail-closed setup failure — a
required output (`--jsonl` / `--capture-dir`) or `--stdin-file` input that could
not be opened — and carries the reserved `SETUP` code (111), distinct from `internal`
(a genuine runner fault) so a consumer never reads a bad path as a runner bug (see
`docs/exit-codes.md`).

This `source` vocabulary has one **mirror**, and it is deliberately a mirror rather
than a fork: a `run` that fails under the global `--error-format json` reports a
`kind` spelled with these same words (`timeout`, `cancelled`, `control_cancel`,
`control_kill`, `output_overflow`, `spawn_error`, `container_error`, `setup`,
`internal` — everything but `child_exit`, which is not a failure). The table above
remains their single source of truth; the envelope exists for the case where there
is no stream to read at all (a run started without `--jsonl`, or a `--detach` that
never got far enough to write one), and a test holds the two spellings together so
they cannot drift. See `docs/exit-codes.md`, "Machine-readable failures:
`--error-format json`".

The "runner process" here is always the process that *ran* the child. Under
`run --detach` that is the detached copy, not the invocation the caller waited on:
the caller's own exit code reports only whether the run started, which is exactly
why this event — unchanged in shape, `source`, or meaning for a detached run — is
where a detached caller reads the child's real result (see `docs/exit-codes.md`,
"Detached runs").

### `output_captured`

Bounded stdout/stderr capture finished. Emitted **only** when `run` was given
`--capture-dir <dir>`: the child's stdout and stderr are teed into
`<dir>/stdout.log` and `<dir>/stderr.log` alongside the live echo — suppressed
when `--no-echo` is also given, which does not change what is captured — and
this event records, per stream, what was captured. A run without `--capture-dir`
does not emit it (the stream is otherwise byte-for-byte identical).
`--inherit-stdio` conflicts with `--capture-dir`, because direct child output
does not pass through the runner's tee, and therefore never emits this event.

| Field    | Type   | Notes                                     |
|----------|--------|-------------------------------------------|
| `stdout` | object | Capture result for standard output (below). |
| `stderr` | object | Capture result for standard error (below).  |

A **capture** object (one per stream):

| Field         | Type    | Notes                                                                                     |
|---------------|---------|-------------------------------------------------------------------------------------------|
| `path`        | string  | The file the stream was written to (`<dir>/stdout.log` or `<dir>/stderr.log`).             |
| `bytes`       | integer | **Full** byte counter — every decoded byte the stream produced; exceeds the file size when the stream was truncated or a write failed. |
| `sha256`      | string  | Lowercase-hex SHA-256 of the bytes actually written to `path` — verify the file against it. Same digest primitive as `argv_sha256`. |
| `truncated`   | boolean | **Explicit** flag: `true` when the stream outran the per-stream capture ceiling and the tail was deliberately not written. Never inferred from the file's size. |
| `write_error` | boolean | **Explicit** flag: `true` when a file write failed part-way through the stream, after which capture stopped writing to the (broken) file. Signals a disk-level problem, distinct from a ceiling clip. Never inferred from the file's size. |

The two explicit flags exist so a consumer distinguishes "captured in full" from
"clipped at the limit" from "cut short by a disk write error" from the flags alone —
not by comparing the file's size against a ceiling it would have to know. The stream
was captured in full exactly when **both** `truncated` and `write_error` are `false`;
then `bytes` equals the file's size and `sha256` covers the whole stream. When
`truncated` is `true`, `bytes` is the full amount produced while the file holds (and
`sha256` covers) the first ceiling's worth. When `write_error` is `true`, a write
failed mid-stream: `bytes` remains the full byte counter, but the file holds — and
`sha256` covers — only the prefix that reached disk before the failure, so `bytes`
exceeds the file's size. The two flags are independent and may both be set (a stream
that outran the ceiling and then also hit a write error). The two streams are likewise
independent: one may be truncated or write-errored while the other is complete. On a
runner-imposed ending (`timeout` / `cancelled`) the event reports whatever was captured
before the teardown.

## Ordering

A normal run emits, in order: `run_started`, `members_snapshot`
(`reason: "spawn"`), then either

- **natural exit** — `root_exited`, `cleanup_started`, `cleanup_finished`,
  `runner_exit`; or
- **runner-imposed ending** — the reason event (`timeout`, `cancelled`, or `killed`),
  `cleanup_started`, `cleanup_finished`, `runner_exit`.

When any resource cap was requested and ProcessGroup creation succeeded, insert
`limit_evidence` after the ending event and before `cleanup_started`. A run
without a cap emits no `limit_evidence`; a pre-spawn `limit_hit` path also emits
none because no group exists to query. The evidence read never moves inside or
after the `cleanup_started`/`cleanup_finished` pair.

The reason event names *which* ending it was: `timeout` (with `reason` `overall` or
`idle`) for a `--timeout` or a `--idle-timeout`, `cancelled` (with `source` `ctrl_c`,
`sigterm`/`sighup` (Unix), `ctrl_break`/`ctrl_close`/`ctrl_logoff`/`ctrl_shutdown`
(Windows), or `control_cancel`) for a local stop signal or a control-plane cancel, and
`killed` (`source` `control_kill`) for a control-plane kill.

**Multiplicity of `members_snapshot`.** The post-spawn `members_snapshot` above is
the only one a run emits *by default*, and it always appears exactly once,
directly after `run_started` — unconditionally, because a failed member read is
recorded as a `read_error` sample rather than omitting the event (see
"members_snapshot"). A run given `run --snapshot-interval <duration>`
emits **additional** `members_snapshot` events (`reason: "interval"`) on that
cadence, all of them after the post-spawn one and all of them **before** the
ending's own event — `root_exited` for a natural exit, or the
`timeout`/`cancelled`/`killed` reason event for a runner-imposed one. None is ever
interleaved into the `cleanup_started`/`cleanup_finished` teardown pair or emitted
after it: the cadence stops the instant the ending is decided. Every other event's
position above is unchanged. This is an additive extension within schema v1 (see
"Versioning"): a consumer that reads the first `members_snapshot` and routes the
rest of the stream by event type is unaffected, and one that wants only the
start-of-run shape can select `reason == "spawn"` instead of relying on position.

When `--capture-dir` is set, an `output_captured` event is inserted after
`cleanup_finished` and before the terminal `runner_exit`, on every ending that ran
the child (natural exit, timeout, cancel, and kill alike). Without `--capture-dir` it
is absent.

A failure before the child is spawned emits its error event (`container_failed`
with `phase` `create` or `attach`, or `spawn_failed`) and then `runner_exit`, with
no `run_started` (and no `output_captured` — the child never produced output). When
that pre-spawn failure is a resource limit that could not be applied, a `limit_hit`
is emitted **first**, immediately before the `container_failed` (`phase` `create`) →
`runner_exit` (`source` `container_error`) pair — the resource-specific record of
why the container could not be created (see `limit_hit`). A
`container_failed` with `phase` `foreground` comes later: the child had already
spawned, so `cleanup_started`/`cleanup_finished` tear the container down before the
terminal `runner_exit`. `run_started` is still never written (the handoff fails
first), and the interactive mode this path only occurs in cannot set
`--capture-dir`, so there is still no `output_captured`.

## Command redaction

Command lines can carry secrets, so `run_started`'s `command` object is redacted
by default (`AGENTS.md`, "Argv is redacted by default"):

| Field         | Type                     | Notes                                                                 |
|---------------|--------------------------|-----------------------------------------------------------------------|
| `redacted`    | boolean                  | `true` by default; `false` only under `--argv-raw`.                   |
| `argv`        | array of string, nullable| The raw argv, present only when `redacted` is `false`; `null` otherwise. |
| `argv_sha256` | string, nullable         | Lowercase-hex SHA-256 fingerprint of argv — see "Fingerprint". Filled on every run. |
| `hint`        | string, nullable         | Worker-shape hint for a recognized argv, else `null` — see "Hint classifier". |

The redaction is deliberately one-directional: `argv_sha256` and `hint` are
derived from argv but cannot reveal it (a one-way hash and a fixed category
label), so they are filled on **every** run — redacted or not. `--argv-raw` *adds*
the raw `argv` array; it never changes the fingerprint or the hint. A consumer can
therefore correlate and classify a run without ever seeing its command line.

Artifact paths follow a separate disclosure rule. The public lifecycle stream does
not add them as event fields: the caller already chose its `--jsonl` and optional
`--capture-dir` destinations. The private, owner-only run registry does publish
those locations as absolute paths so a different supervisor can discover a detached
run's observability artifacts. Paths are not derived from argv, but may contain a
project or user name; treat `list --json` and `inspect --json` as private operational
metadata and do not copy them into public logs. See
[`docs/registry.md`](registry.md#which-run-is-which--and-what-a-record-never-carries).

### Fingerprint (`argv_sha256`)

`argv_sha256` is the SHA-256 of a canonical encoding of argv, rendered as
lowercase hex (64 characters). The canonical encoding is **the argv elements
joined by a single NUL byte (`0x00`)** — each element as its UTF-8 bytes, with no
leading or trailing separator and no terminator. A NUL cannot occur inside a real
argv element on any supported platform, so element boundaries are unambiguous:
`["ab", "c"]` and `["a", "bc"]` fingerprint differently. An adapter that re-emits
this schema reproduces the exact digest by hashing the same encoding. (The
reference implementation is `events::argv_sha256_hex`.)

### Hint classifier

`hint` names a recognized *worker shape* — a process form worth identifying (for
example a build worker left running after a build) without disclosing its command
line. It is one of a small, documented catalog of category labels, or `null` when
the argv matches no known shape (the common case). A rule matches when **all** of
its marker substrings appear somewhere in the argv, compared case-insensitively;
the first matching rule in catalog order wins.

| `hint`               | Markers (all must be present)                    | Shape |
|----------------------|--------------------------------------------------|-------|
| `msbuild_node_reuse` | `MSBuild.dll`, `/nodemode:1`, `/nodeReuse:true`  | An MSBuild reusable worker node (`/nodeReuse:true`) — the long-lived build-node process that lingers after a build. |

**Adding a shape.** The catalog is plain data — the `HINT_RULES` table in
`src/events.rs`. Add one entry (a new `hint` label plus the marker substrings that
identify the shape) and mirror it as a row in the table above; no control-flow
change is needed. Choose a stable, snake_case `hint` label: consumers may match on
it, so an existing label is part of this contract (changing or removing one is a
breaking change — see "Versioning"). Keep the label to ASCII lowercase letters,
digits, and `_` — the per-user run registry publishes the same label in its records
and validates that shape when reading one back
([`docs/registry.md`](registry.md), "Reading a record"); a label outside it would be
dropped there. An in-tree test asserts every catalog label satisfies this, so a rule
that violates it fails the build rather than surfacing as an empty column in `list`.

## Enriched member fields

`ppid`, executable `name`, and `start_time` are filled from ProcessKit's
`ProcessGroup::members_info()` — built strictly on the public `processkit` API
rather than a local process-enumeration path (`AGENTS.md`, "Build strictly on the
public `processkit` API"). Each field stays independently nullable because
`members_info()` itself reports a field `null` wherever the platform can't read
it: on Windows, Linux (cgroup or the process-group fallback), and macOS every
field is populated; on the "bare" BSDs (no wired-up per-process reader) every
enriching field is `null` while `pid` is still reported — a correct result, not
an error. A member that exits between enumeration and metadata read is omitted
from `members` entirely rather than reported with fabricated fields.

`start_time` is **not** a wall-clock timestamp — it is an opaque, platform- and
unit-specific token (Windows: 100 ns since 1601-01-01 UTC; Linux: clock ticks
since boot; macOS: microseconds since the Unix epoch) whose only documented
purpose is telling a recycled `pid` apart from the process that previously held
it. It is rendered as its decimal string, matching the field's `string, nullable`
type, and must never be parsed as a timestamp or compared across platforms.

The same enrichment backs the control-plane `inspect` snapshot (see
[`docs/control-plane.md`](control-plane.md)): both `members_snapshot` and
`inspect`'s `members` are queried through `members_info()`, so the two "container
member" views never drift.

## Versioning

`schema_version` is a single integer. Any **breaking** change to an event's shape
— renaming/removing a field, changing a field's type, or changing the meaning of a
value — is a **major** bump of `schema_version` (and a matching `Cargo.toml`
version bump; `docs/exit-codes.md` and `AGENTS.md` treat the surface as a whole).
A future version lands under a new `fixtures/schema/vN/` directory. Additive,
backward-compatible clarifications that do not change any existing shape do not bump
the version. Adding a **new event type** (as `output_captured` was added) is
likewise additive: it introduces no change to any existing event's shape, and a
consumer that pins the events it knows simply ignores one it does not. Adding a **new
field** to an existing event — always present, and leaving every other field's name,
type, and meaning intact — is additive in the same way: a consumer that reads the
fields it knows is unaffected and simply ignores the new one. The `output_captured`
per-stream `write_error` flag was added this way within v1, as was the `timeout`
event's `reason` field (when `--idle-timeout` joined `--timeout` on that event) and
the `members_snapshot` event's own `reason` and `read_error` fields (when
`--snapshot-interval` gave that event a second trigger, and a failed sample a way to
report itself). Note what that does *not* mean: those two fields appear on **every**
`members_snapshot`, including the single one a run without the flag emits, so the
default stream's `members_snapshot` line is not byte-identical to what an earlier
version wrote. That is precisely the additive case above — a reader consuming the
fields it knows is unaffected — but a reader that pinned the exact field set of an
event, rather than the fields it uses, will notice.
Emitting an **existing event more times** than a
stream previously carried it is additive for the same reason a new event type is:
no existing event's shape changes, and a consumer pinned to a version must already
tolerate events it did not expect at that point in the stream — within a version it
may not assume an event type it knows occurs at most once. `members_snapshot` was
extended this way within v1 by the opt-in `run --snapshot-interval` cadence, which
also leaves the default stream (no flag) emitting exactly the events, in exactly the
order and the number, that it did before; the normative statement of how many such
events a stream may carry, and where they may appear, lives in "Ordering", and the
reader obligations these two additive shapes create are collected for adapter
authors in [Compatibility and upgrades](compatibility.md#schema-pinning).
Adding a **new value** to an open-ended
descriptive string field — a new `cancelled` `source`, for instance, as `sigterm` and
`sighup` were added within v1 when the runner started catching those signals — is
additive too: no existing value changes meaning, every other field keeps its name,
type, and meaning, and a consumer that switches on the values it knows sees a
well-formed event it can still route by event type (treat an unknown `source` as "some
other trigger", not as a parse error). Filling a
field that was reserved-as-`null` is **not** a breaking change: the field already
exists and its type is unchanged. The `argv_sha256` and
`hint` fields were filled this way — they now carry values on every run instead of
always `null`; the enriched member fields (see "Enriched member fields" above) were
filled the same way once ProcessKit shipped `members_info()`. Adding a new `hint` label to the classifier
catalog is likewise additive, but renaming or removing an existing `hint` label, or
changing the fingerprint's canonical encoding, changes the meaning of a value and
so is a breaking change.
