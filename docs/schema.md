# JSONL event schema (v1)

This is the **normative description** of processkit-cli's JSONL lifecycle-event
contract. It is part of the project's public compatibility surface — *CLI flags +
exit-code ranges + `schema_version`* (see `AGENTS.md`) — because adapters, in
particular the processkit-py CLI, pin `schema_version` and reimplement these
shapes. Treat every field below as public API.

- The in-code source of truth is `src/events.rs`.
- The golden sample stream is
  [`fixtures/schema/v1/events.jsonl`](../fixtures/schema/v1/events.jsonl); the
  golden test (`events::tests::golden_stream_matches_the_fixture`) keeps this
  document, the code, and the fixture in lockstep.

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

| Field       | Type              | Notes                                                                 |
|-------------|-------------------|-----------------------------------------------------------------------|
| `run_id`    | string            | The `--run-id` value, or a generated `run-<pid>-<unix_nanos>`.         |
| `root_pid`  | integer, nullable | The root child's PID; `null` if the backend exposed none.             |
| `mechanism` | string            | Containment mechanism: `job_object`, `cgroup_v2`, or `process_group`. |
| `cwd`       | string, nullable  | The child's working directory; `null` if it could not be resolved.    |
| `command`   | object            | The command, redacted by default — see "Command redaction".           |

### `members_snapshot`

A point-in-time snapshot of the container's members. It is a snapshot, not a
census: a listed PID may exit immediately afterward, and a process spawned during
the read may be missing.

| Field     | Type            | Notes                              |
|-----------|-----------------|------------------------------------|
| `members` | array of member | Each entry is a *member* (below).  |

A **member** object:

| Field        | Type              | Notes                                                    |
|--------------|-------------------|----------------------------------------------------------|
| `pid`        | integer           | The process id.                                          |
| `ppid`       | integer, nullable | Parent pid — see "Enriched member fields".               |
| `name`       | string, nullable  | Executable name — see "Enriched member fields".          |
| `start_time` | string, nullable  | Process start time — see "Enriched member fields".       |

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

### `cleanup_finished`

Container teardown finished (after the hard kill).

| Field            | Type              | Notes                                                                   |
|------------------|-------------------|-------------------------------------------------------------------------|
| `remaining`      | integer           | Count of `remaining_pids`.                                              |
| `remaining_pids` | array of integer  | Post-kill member snapshot; normally empty.                             |
| `soft_terminate` | string, nullable  | The soft-stop tier for a runner-imposed ending (below); `null` on the natural-exit path. |

`remaining_pids` is a snapshot: on the Job Object and cgroup mechanisms a process
leaves membership on exit, so it is empty after the kill; on the POSIX
process-group fallback an unreaped just-exited child can still be listed until it
is reaped. `soft_terminate` is one of:

- `signalled` — a real soft signal (`SIGTERM`) was delivered to the tree (Unix).
- `unsupported` — the platform has no soft-terminate tier yet (Windows); nothing
  was sent, and the runner does not pretend otherwise. The grace window still
  elapsed before the atomic Job Object kill.
- `failed` — the soft signal could not be delivered; the hard kill ran regardless.

### `limit_hit`

A configured ProcessKit resource limit (process count, memory, CPU) was exceeded
or could not be applied.

| Field    | Type             | Notes                                            |
|----------|------------------|--------------------------------------------------|
| `limit`  | string           | Which limit, e.g. `processes`, `memory`, `cpu`.  |
| `detail` | string, nullable | Human-readable detail; `null` if none.           |

**Status in v1.** This event's shape is fixed now so adapters can pin it, but the
runner exposes no resource-limit configuration yet, so it is not emitted at
runtime in this version. It is reserved for when limit flags land, at which point
the runner emits it without changing the shape above.

### `timeout`

The `--timeout` deadline elapsed while the child was still running. The teardown
it triggers is described by the following `cleanup_started` / `cleanup_finished`
events; the run's terminal code is the reserved `TIMEOUT` (106) — see
`docs/exit-codes.md`.

| Field        | Type              | Notes                                     |
|--------------|-------------------|-------------------------------------------|
| `timeout_ms` | integer           | The deadline that elapsed, milliseconds.  |
| `grace_ms`   | integer, nullable | The `--grace` window, ms; `null` if unset. |

### `cancelled`

The run was cancelled interactively (`Ctrl-C`). Terminal code is the reserved
`CANCELLED` (107).

| Field      | Type              | Notes                                      |
|------------|-------------------|--------------------------------------------|
| `source`   | string            | `ctrl_c`.                                  |
| `grace_ms` | integer, nullable | The `--grace` window, ms; `null` if unset. |

### `spawn_failed`

The program could not be started (not found, not executable, bad `--cwd`): the
child never ran.

| Field     | Type    | Notes                                          |
|-----------|---------|------------------------------------------------|
| `code`    | integer | The runner-band exit code (`SPAWN`, 101).      |
| `message` | string  | Human-readable failure reason.                 |

### `container_failed`

Creating the container, or joining the child to it, failed.

| Field     | Type    | Notes                                                                 |
|-----------|---------|-----------------------------------------------------------------------|
| `phase`   | string  | `create` (the container could not be created) or `attach` (the launch into it failed). |
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
| `source`     | string            | Why the runner exited: `child_exit`, `timeout`, `cancelled`, `spawn_error`, `container_error`, or `internal`. |
| `child_code` | integer, nullable | The child's own exit code when it exited on its own; `null` for a runner-imposed ending or a child that never produced one. |

When `source` is `child_exit`, `code` equals `child_code`. For a runner-imposed
ending (`timeout` / `cancelled`) or a pre-run failure (`spawn_error` /
`container_error`), `child_code` is `null` and `code` is the runner-band value.

## Ordering

A normal run emits, in order: `run_started`, `members_snapshot`, then either

- **natural exit** — `root_exited`, `cleanup_started`, `cleanup_finished`,
  `runner_exit`; or
- **runner-imposed ending** — `timeout` *or* `cancelled`, `cleanup_started`,
  `cleanup_finished`, `runner_exit`.

A failure before the child runs emits its error event (`container_failed` or
`spawn_failed`) and then `runner_exit`, with no `run_started`.

## Command redaction

Command lines can carry secrets, so `run_started`'s `command` object is redacted
by default (`AGENTS.md`, "Argv is redacted by default"):

| Field         | Type                     | Notes                                                                 |
|---------------|--------------------------|-----------------------------------------------------------------------|
| `redacted`    | boolean                  | `true` by default; `false` only under `--argv-raw`.                   |
| `argv`        | array of string, nullable| The raw argv, present only when `redacted` is `false`; `null` otherwise. |
| `argv_sha256` | string, nullable         | Hex SHA-256 of argv — see below.                                      |
| `hint`        | string, nullable         | Classified worker-shape hint (e.g. an MSBuild node-reuse shape) — see below. |

**Status in v1.** The redaction *machinery* — computing `argv_sha256` and the
worker-shape `hint` classifier — lands in a later task (T-005). The fields are
declared now so the event shape does not change when that machinery arrives: in
v1 `argv_sha256` and `hint` are always `null`, and `--argv-raw` already records
`argv` verbatim. T-005 fills the two reserved fields without reshaping the event.

## Enriched member fields

ProcessKit's public API returns bare PIDs today; the richer per-member snapshot
(`ppid`, executable `name`, `start_time`) is a filed-but-unshipped ProcessKit-rs
capability (`ProcessGroup::members_info()`). Rather than reimplement process
enumeration locally (`AGENTS.md`, "Build strictly on the public `processkit`
API"), v1 declares those fields as nullable and fills them from bare PIDs alone —
so today they are always explicitly `null`. When the core capability ships, the
runner populates them without reshaping the event.

## Versioning

`schema_version` is a single integer. Any **breaking** change to an event's shape
— renaming/removing a field, changing a field's type, or changing the meaning of a
value — is a **major** bump of `schema_version` (and a matching `Cargo.toml`
version bump; `docs/exit-codes.md` and `AGENTS.md` treat the surface as a whole).
A future version lands under a new `fixtures/schema/vN/` directory. Additive,
backward-compatible clarifications that do not change any existing shape do not bump
the version. Filling a field that was reserved-as-`null` (the T-005 argv machinery,
the enriched member fields) is **not** a breaking change: the field already exists
and its type is unchanged.
