# Live-run control plane

The **control plane** lets a client query and (later) steer a *running*
`processkit-cli run`. It lives in the **live runner process**, not in named kernel
objects (`AGENTS.md`, "The control plane lives in the live runner process"): a runner
must stay alive to hold its kill-on-drop container, so the live process is exactly
where clients reach it. If the runner dies, the container tears the tree down and the
run becomes **detectably gone** — never a dangling handle a client could act on by
mistake.

This document is the normative description of the **local transport**, the **wire
protocol**, and the three clients — **`inspect`** (read-only) and the mutating
**`cancel`** / **`kill`** — including their behavior when the runner cannot be reached.
Discovery — how a client *finds* a live runner — is the run registry, described in
[`docs/registry.md`](registry.md). The in-code source of truth is `src/control/`.

`cancel` and `kill` add verbs to the **same** transport and protocol as `inspect`
without reshaping either: one request verb line in, one JSON line out, connection
closed. They are *mutating* — they end the live run — and they reuse the run's own
teardown path (the same one a `--timeout` or a `Ctrl-C` drives, see
[`docs/schema.md`](schema.md)), so a control command never invents a second way to
kill a tree.

## Discovery: the registry, never a PID

A client never addresses a run by PID (`AGENTS.md`: "Nothing is addressed by PID").
It finds one through the per-user run registry: it scans records, matches the target
`run_id`, and acts only on a **live** entry (see [`docs/registry.md`](registry.md),
"Staleness"). A record's `endpoint` field carries the address of that run's local
transport — the channel this document describes.

The registry does not enforce `run_id` uniqueness, so more than one live entry can
match. That is an **ambiguous run id** — a hard `CONTROL` (103) failure for every
verb (`inspect` included), never a silent pick of whichever entry the scan returns
first. See [`docs/registry.md`](registry.md), "Run id resolution — ambiguity is a
hard failure".

## Local transport

Each run stands up one local IPC endpoint, restricted to the current user, and
publishes its address in the run's registry record:

- **Unix:** a **unix domain socket**. The socket file is created in a short per-run
  owner-only (`0700`) directory under `/tmp` (with the platform temp directory as a
  fallback), and its own mode is tightened to `0600`. The short path is independent
  of the registry location so deeply nested CI/project paths cannot exceed macOS's
  `sun_path` limit. The endpoint address is the socket's absolute path.
- **Windows:** a **named pipe** (`\\.\pipe\processkit-cli-<unique>`), created with a
  **protected** DACL that grants full access to the current user alone
  (`D:P(A;;FA;;;<current-user-SID>)`, built from the same SID the registry restricts
  to), created with `FILE_FLAG_FIRST_PIPE_INSTANCE` (so no other process can pre-own
  the name), and rejecting remote clients. The endpoint address is the pipe name.

Both are locked to the same single user as the registry, because an endpoint is a
control channel — a world-reachable one would hand it to any local process.

### Concurrency, and never blocking the run

The transport is served **concurrently with the child's output pump**, on the same
runtime. It never blocks the happy path:

- A live run that no one inspects pays only an idle accept.
- The run's exit and teardown do **not** wait on any control client. When the child
  exits (or a `--timeout` / `Ctrl-C` ends the run), the run resolves and the control
  server is dropped along with it — tearing the transport down. The child's exit-code
  fidelity is never at the mercy of a slow or absent control client.

The transport is **best-effort infrastructure**: if it cannot be stood up, the runner
warns on stderr, records a `null` endpoint, and runs the child normally — the run is
simply not inspectable. Losing it never costs the child its faithfully forwarded exit
code (`AGENTS.md`, "Exit-code fidelity").

### Cleanup and leaks

On a clean teardown (a normal child exit, a `--timeout`, or a `Ctrl-C`) the transport
is torn down with the run — on unix the socket file and its private directory are
removed. An **abrupt** runner death (crash, `SIGKILL`, a parent's Job Object
terminate) skips that removal, stranding the socket directory exactly as it strands
the registry record and lock. The leak is inert while it lasts: a client detects the
run as stale through the registry *before* it ever connects, so it never touches the
orphaned socket.

It does not last, either. `prune` reaps all three together: reaping a
**confirmed-stale** record now also removes the `pkc-…` directory and socket that
record published, so an abrupt death no longer accumulates dead socket directories in
`/tmp` — see [`docs/registry.md`](registry.md), "Reaping — `prune`", for the shape
check that endpoint has to pass first (it is untrusted data, like everything else in
a record) and for what the reaper deliberately refuses to touch. On Windows there is
nothing to reap: the pipe simply vanishes with the process.

## Wire protocol

Line-oriented and deliberately tiny. Over an accepted connection:

1. The client writes one **request verb** line, UTF-8, terminated by `\n`. The verbs
   are `inspect`, `cancel`, and `kill`. (An empty line is also treated as `inspect`,
   so a bare connect-and-read probe still works.)
2. The server writes back **one JSON line** — the response — and closes the
   connection.

The responses per verb:

| Verb      | Response line                                | Effect on the run                                                        |
|-----------|----------------------------------------------|--------------------------------------------------------------------------|
| `inspect` | a [snapshot](#the-inspect-snapshot)          | none (read-only).                                                        |
| `cancel`  | an [ack](#cancel-and-kill) `{"accepted":true,"action":"cancel","run_id":"…"}` | the run runs its shared soft-stop → grace → hard-kill teardown and exits with `CONTROL_CANCELLED` (108). |
| `kill`    | an [ack](#cancel-and-kill) `{"accepted":true,"action":"kill","run_id":"…"}`   | the run hard-kills the whole tree immediately (no soft stop, no grace) and exits with `CONTROL_KILLED` (109). |

An unrecognized verb yields a JSON error object (`{"error":"..."}`) instead, and
changes nothing about the run — a foreign client cannot end a run by sending garbage.

For the mutating verbs the runner writes its **ack first**, then signals its own main
loop to tear down. The client therefore always receives its confirmation even though
the run ends the instant the signal lands; and if the ack cannot even be written (a
broken client), no teardown is signaled — an unconfirmed cancel never silently ends a
run.

## `inspect`

```
processkit-cli inspect (--run-id <id> [--json] | --all --json [--label <KEY=VALUE>]...)
```

`inspect` finds the live runner for `<id>` through the registry, connects to its
endpoint, sends the `inspect` verb, and prints the snapshot to **stdout** — as a
single JSON line with `--json`, or, by default, as a human-readable rendering
(snapshot version, run id, mechanism, root pid, start time, artifact locators, and a member table),
mirroring `list`/`prune`'s optional `--json`. `--json` is optional; `inspect --json`'s
output is unchanged from before `--json` became optional.

The aggregate form requires `--json`. It takes one snapshot of all confirmed-live
registry records, optionally filters them by repeated exact `--label KEY=VALUE`
matches (logical AND), and addresses each target by the record path and endpoint
captured in that snapshot. It prints one JSON array. Each element has `run_id` and
either a `snapshot` with `error: null`, or `snapshot: null` with a bounded error
string. If any target fails, the command returns `CONTROL` (103) after printing the
complete array; an empty matching fleet is a successful empty array. This preserves
partial fleet visibility without turning registry churn into silent omission.

### The inspect snapshot

The snapshot is the machine-readable state of a live run. It is the control plane's
own client/runner contract, versioned on its own axis (`snapshot_version`), distinct
from the JSONL event `schema_version` and the `registry_version`.

| Field              | Type              | Notes                                                                 |
|--------------------|-------------------|-----------------------------------------------------------------------|
| `snapshot_version` | integer           | Snapshot format version (currently `2`).                              |
| `run_id`           | string            | The run's identifier — the key matched in the registry. Not a PID.    |
| `mechanism`        | string            | Containment mechanism: `job_object`, `cgroup_v2`, or `process_group` (same vocabulary as the JSONL `run_started`). |
| `root_pid`         | integer, nullable | The root child's PID; `null` if the backend exposed none.             |
| `started_at`       | string            | Run start time, RFC 3339 UTC, millisecond precision.                  |
| `jsonl`            | string, nullable  | Absolute path to the JSONL lifecycle stream; `null` only when reading a legacy snapshot. |
| `capture_dir`      | string, nullable  | Absolute capture directory, or `null` when capture is disabled.       |
| `members`          | array of member   | The container's members, enriched with `ppid`/executable `name`/`start_time` wherever ProcessKit's `members_info()` can report them — the same shape as the JSONL `members_snapshot` (`docs/schema.md`, "Enriched member fields"), and read through the same call, so the two views never drift. Fields stay `null` on platforms/members that can't report them (e.g. the "bare" BSDs). Queried **at request time**, so it reflects the container's composition *when inspected*, not at start. |

Example:

```json
{"snapshot_version":2,"run_id":"build-42","mechanism":"job_object","root_pid":4242,"started_at":"2026-07-20T21:00:00.000Z","jsonl":"C:\\runs\\build-42.jsonl","capture_dir":null,"members":[{"pid":4242,"ppid":4200,"name":"build.exe","start_time":"133456789000000000"}]}
```

## `cancel` and `kill`

```
processkit-cli cancel (--run-id <id> | --all [--label <KEY=VALUE>]...)
processkit-cli kill   (--run-id <id> | --all [--label <KEY=VALUE>]...)
```

`--run-id` and `--all` are mutually exclusive and exactly one is required, the same
clap convention [`docs/registry.md`](registry.md#waiting--wait)'s `wait --all`
(T-216) established — a bare `cancel`/`kill` with neither is a `USAGE` (100) form
error at parse time.

`--run-id <id>` finds the live runner for `<id>` through the registry exactly as
`inspect` does — by matching `run_id`, **never** a PID — connects to its endpoint,
sends the verb, and ends the run. `cancel` and `kill` differ only in *how* the run is
ended:

- **`cancel`** asks the runner to run its **shared** soft-stop → grace → hard-kill
  teardown — the same path a `--timeout` or a `Ctrl-C` drives. On Unix a real
  `SIGTERM` is delivered to the tree, the `--grace` window (if the run was started
  with one) elapses, and the container's kill-on-drop then hard-tears-down whatever
  remains. On Windows a Job Object has no POSIX signal, so the soft tier is
  `WM_CLOSE` to windowed members plus `CTRL_BREAK` for a child launched with
  `--windows-graceful-ctrl-break`; a capability probe reports when neither target
  exists, and ProcessKit then escalates atomically. The run
  exits with the reserved **`CONTROL_CANCELLED` (108)**.
- **`kill`** hard-kills the whole tree **immediately**: no soft stop, no grace. The
  run exits with the reserved **`CONTROL_KILLED` (109)**.

The scope of either is **only** the target run's container, discovered by `run_id`
through the registry. Nothing is ever killed by executable name, and no process
outside the run's own ProcessKit container is touched.

### The ack

On success the runner replies with one JSON line — an **ack** — and the client prints
it to **stdout** before exiting `0`:

| Field      | Type    | Notes                                                              |
|------------|---------|--------------------------------------------------------------------|
| `accepted` | boolean | `true` — the runner accepted the command and began tearing down.   |
| `action`   | string  | The action taken: `cancel` or `kill` (echoed so the client can confirm the runner answered the verb it sent). |
| `run_id`   | string  | The run the command targeted.                                      |

```json
{"accepted":true,"action":"cancel","run_id":"build-42"}
```

The client parses the ack back and checks it names the action it asked for; a rejected
or garbled reply is treated as an unreachable-runner failure (below), never a false
success.

### The outcome is visible to any observer, not just the client

The client's ack is not the only record of the command. The run also writes the
outcome to its JSONL stream (`--jsonl`), so an **external** observer reading the event
file — not the control client — still sees that the run ended by an outside command:

- **`cancel`** writes a `cancelled` event with `source` **`control_cancel`** (told
  apart from the local stop signals, which are `ctrl_c` / `sigterm` / `sighup` (Unix)
  / `ctrl_break` / `ctrl_close` / `ctrl_logoff` / `ctrl_shutdown` (Windows)), the
  `cleanup_started` / `cleanup_finished`
  teardown pair, and a terminal `runner_exit` with `source` `control_cancel` and code
  `108`.
- **`kill`** writes a dedicated `killed` event with `source` **`control_kill`**, the
  cleanup pair (with `soft_terminate` `null` — no soft stop was attempted), and a
  terminal `runner_exit` with `source` `control_kill` and code `109`.

See [`docs/schema.md`](schema.md) for these events.

### `cancel --all` / `kill --all`

`--all` is the aggregate counterpart to `--run-id`: instead of one named run,
it acts on **every registry record confirmed live in a snapshot taken the moment the invocation
starts**. Repeated `--label KEY=VALUE` filters narrow that snapshot with logical AND;
only records carrying every exact pair remain, and labels are rejected with the
by-id form. This is the mutating counterpart to `wait --all` (T-216; see
[`docs/registry.md`](registry.md), "Waiting — `wait`", "The aggregate barrier —
`wait --all`"), reusing its exact snapshot discipline. The target set is fixed once, before
the first mutation is dispatched: a run that registers *after* the snapshot is out of
scope for this invocation, and a run that is only `unprobed` (not confirmed live) *at
that instant* is excluded from the snapshot outright, the same asymmetry `wait --all`
documents. Each target is keyed by its unique registry-record path and the endpoint
that exact record advertised at snapshot time, never by its potentially duplicated
`run_id`. `--all` can therefore reach two live records sharing an id independently,
while the by-`run-id` form remains the hard ambiguity described above. Immediately
before dispatch the client re-reads and probes that exact record path, without
scanning or probing unrelated entries, and requires it to remain live with the same
id and endpoint; only then does it use the ordinary [wire exchange](#wire-protocol).

An **empty snapshot** (no confirmed-live entry at all — an empty or fully-stale
registry) is not an error, mirroring `prune`: it prints an empty report (`[]`) and
exits `0`. Opening or scanning the registry itself failing (not "found nothing", but
"could not even look") is a [`exit::SETUP`](exit-codes.md) (111) failure, the same
support/prerequisite failure `list`/`prune`/`wait` report for the identical condition
— distinct from the single-run form's `CONTROL` (103), since there is no one target's
reachability in question yet at that point.

**The report.** Instead of the single-run form's one [ack](#cancel-and-kill) object,
`--all` prints one JSON array, one entry per snapshot target, to **stdout**:

| Field      | Type              | Notes                                                                 |
|------------|-------------------|------------------------------------------------------------------------|
| `run_id`   | string            | The target record's descriptive run id; not its aggregate identity key. |
| `accepted` | boolean           | Whether the runner acknowledged this invocation's mutation.             |
| `status`   | string            | `accepted`, `already_gone`, or `failed`.                                |
| `error`    | string, omitted unless failed | Present only for `failed`; names why the still-potentially-live target could not be safely reached or did not acknowledge. |

```json
[{"run_id":"build-42","accepted":true,"status":"accepted"},{"run_id":"build-43","accepted":false,"status":"already_gone"}]
```

A target that disappears or becomes confirmed stale between the snapshot and its
dispatch is reported as `already_gone`: no runner acknowledged the verb, so
`accepted` remains false, but the aggregate's terminal-state goal is already met and
the outcome is non-error. An entry that becomes unprobeable, changes identity, cannot
be reached while still confirmed live, or rejects/mismatches its ack is `failed` and
does not stop fan-out to the remaining targets. `--all` never skips a snapshot entry
silently: every one gets exactly one array entry.

**The aggregate exit code.** Full success — every snapshot target is either
`accepted` or `already_gone` — is `0`. A **partial or full failure** is never a silent
`0`: it reuses the reserved **`CONTROL` (103)** code (the same one the single-run form
uses for "could not reach the target run" — there being one or more unreachable
targets is the same class of fact for the aggregate), with a summary message on
stderr naming how many of the snapshot targets failed; the full per-target detail is
only in the JSON report on stdout, printed **before** that failing exit. A caller
that needs `--all` to fail loudly on any partial failure (the typical teardown
sequence — `cancel --all` before `wait --all`/`prune`) gets that for free from the
non-zero exit; one that wants the detail parses the report.

**Skipped entries — unchanged from the single-run form.** A registry entry that is
`stale` or `unprobed` at snapshot time is never in the target set at all (`--all`
acts only on entries [`Health::Live`](registry.md) confirms), exactly the same bar
the single-run form's own resolver applies — `--all` only distributes that existing
rule over a snapshot, it never widens or narrows it.

### When the runner cannot be reached: a distinguishable result, never a hang

Every client — `inspect`, `cancel`, and `kill` — can lose the runner the same three
ways (this applies per target under `--all` too, one snapshot entry at a time). All
of them are reported as the reserved **`CONTROL` exit code (103)** — "could
not reach the target run" (see [`docs/exit-codes.md`](exit-codes.md)) — with an
explanatory message on **stderr** (naming the action and the run) and nothing on
stdout for the single-run form (under `--all`, the same message text lands in that
target's `error` field in the report instead). None is a generic error, and none
hangs:

- **Stale registry entry.** The runner died abruptly, leaving its record behind; the
  released liveness lock makes the entry stale. The client detects this *before*
  connecting and reports the run as gone (its registry entry is stale).
- **Unprobeable registry entry.** The liveness probe could not be performed at all —
  the entry's lock file would not open (a directory in its place, a permission error,
  a rejected symlink/reparse point), the same case `list` prints as `unprobed` and
  `prune` refuses to reap (see [`docs/registry.md`](registry.md#the-reaping-safety-invariant)).
  The client refuses just as it does for a stale entry — it acts only on a
  **confirmed-live** match, and this is not one — but it says so differently: the
  message reports that liveness could not be probed and names the entry `unprobed`,
  never that the runner is gone, which is a confirmed death nothing established. So a
  refusal you cross-check against `list` will always agree with what `list` shows for
  that record.
- **Died mid-conversation.** The entry read live, but the runner exited between the
  liveness probe and the reply — so the connect fails, or the connection closes before
  a complete response arrives. The client reports that the runner could not be reached
  or closed the connection before answering.

Every wait — connecting, and the whole request/response exchange — is bounded by a
deadline, so a runner that accepts a connection but never answers cannot wedge the
client either; it, too, ends as a bounded `CONTROL` failure. A run id that is not
registered at all is likewise a `CONTROL` failure naming the missing run.

For the mutating verbs this matters twice over: a `cancel`/`kill` against a run that
is already gone is the *same* bounded `CONTROL` (103) result — it never blocks waiting
for a teardown that will not happen, and it does not mistake a dead run for a
successful cancel.

This is the exit-code half of the contract: a caller distinguishes "here is the run's
state" / "the command was accepted" (exit `0`, JSON on stdout) from "that run is not
reachable" (exit `103`, message on stderr) without parsing free text.
