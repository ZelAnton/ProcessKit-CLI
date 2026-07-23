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
- A machine-readable JSON Schema (draft 2020-12) is published at
  [`fixtures/schema/v1/schema.json`](../fixtures/schema/v1/schema.json) — one
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
| `root_pid`       | integer, nullable | The root child's PID; `null` if the backend exposed none.             |
| `mechanism`      | string            | Containment mechanism: `job_object`, `cgroup_v2`, or `process_group`. |
| `abrupt_cleanup` | string            | Cleanup surviving abrupt runner death: `whole_tree`, `direct_child_only`, or `none`. |
| `cwd`            | string, nullable  | The child's working directory; `null` if it could not be resolved.    |
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

The run was cancelled and torn down through the shared soft-stop → grace → hard-kill
path. `source` names the trigger, and the terminal `runner_exit` carries the matching
reserved code:

- `ctrl_c` — a local interactive `Ctrl-C`; terminal code `CANCELLED` (107).
- `control_cancel` — a `cancel` command that reached the live runner over its control
  plane (see [`docs/control-plane.md`](control-plane.md)); terminal code
  `CONTROL_CANCELLED` (108).

Both share this event because they share the teardown; only the `source` and the
terminal code tell them apart.

| Field      | Type              | Notes                                            |
|------------|-------------------|--------------------------------------------------|
| `source`   | string            | `ctrl_c` or `control_cancel`.                    |
| `grace_ms` | integer, nullable | The `--grace` window, ms; `null` if unset.       |

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
| `source`     | string            | Why the runner exited: `child_exit`, `timeout`, `cancelled`, `control_cancel`, `control_kill`, `spawn_error`, `container_error`, `internal`, or `setup`. |
| `child_code` | integer, nullable | The child's own exit code when it exited on its own; `null` for a runner-imposed ending or a child that never produced one. |

When `source` is `child_exit`, `code` equals `child_code`. For a runner-imposed
ending (`timeout` / `cancelled` / `control_cancel` / `control_kill`) or a pre-run
failure (`spawn_error` / `container_error` / `setup`), `child_code` is `null` and
`code` is the runner-band value. `setup` names a fail-closed setup failure — a
required output (`--jsonl` / `--capture-dir`) or `--stdin-file` input that could
not be opened — and carries the reserved `SETUP` code (111), distinct from `internal`
(a genuine runner fault) so a consumer never reads a bad path as a runner bug (see
`docs/exit-codes.md`).

### `output_captured`

Bounded stdout/stderr capture finished. Emitted **only** when `run` was given
`--capture-dir <dir>`: the child's stdout and stderr are teed into
`<dir>/stdout.log` and `<dir>/stderr.log` alongside the unchanged live echo, and
this event records, per stream, what was captured. A run without `--capture-dir`
does not emit it (the stream is otherwise byte-for-byte identical).

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

A normal run emits, in order: `run_started`, `members_snapshot`, then either

- **natural exit** — `root_exited`, `cleanup_started`, `cleanup_finished`,
  `runner_exit`; or
- **runner-imposed ending** — the reason event (`timeout`, `cancelled`, or `killed`),
  `cleanup_started`, `cleanup_finished`, `runner_exit`.

The reason event names *which* ending it was: `timeout` for a `--timeout`, `cancelled`
(with `source` `ctrl_c` or `control_cancel`) for a Ctrl-C or a control-plane cancel,
and `killed` (`source` `control_kill`) for a control-plane kill.

When `--capture-dir` is set, an `output_captured` event is inserted after
`cleanup_finished` and before the terminal `runner_exit`, on every ending that ran
the child (natural exit, timeout, cancel, and kill alike). Without `--capture-dir` it
is absent.

A failure before the child runs emits its error event (`container_failed` or
`spawn_failed`) and then `runner_exit`, with no `run_started` (and no
`output_captured` — the child never produced output).

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
breaking change — see "Versioning").

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
the version. Adding a **new event type** (as `output_captured` was added) is
likewise additive: it introduces no change to any existing event's shape, and a
consumer that pins the events it knows simply ignores one it does not. Adding a **new
field** to an existing event — always present, and leaving every other field's name,
type, and meaning intact — is additive in the same way: a consumer that reads the
fields it knows is unaffected and simply ignores the new one. The `output_captured`
per-stream `write_error` flag was added this way within v1. Filling a
field that was reserved-as-`null` is **not** a breaking change: the field already
exists and its type is unchanged. The `argv_sha256` and
`hint` fields were filled this way — they now carry values on every run instead of
always `null`; the enriched member fields remain reserved and `null` until
ProcessKit ships `members_info()`. Adding a new `hint` label to the classifier
catalog is likewise additive, but renaming or removing an existing `hint` label, or
changing the fingerprint's canonical encoding, changes the meaning of a value and
so is a breaking change.
