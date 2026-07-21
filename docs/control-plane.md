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
**`cancel`** / **`kill`** — including their behavior when the runner is gone.
Discovery — how a client *finds* a live runner — is the run registry, described in
[`docs/registry.md`](registry.md). The in-code source of truth is `src/control.rs`.

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
terminate) skips that removal, leaking the socket directory exactly as it leaks the
registry record and lock. That leak is inert: a client detects the run as stale
through the registry *before* it ever connects, so it never touches the orphaned
socket. On Windows the pipe simply vanishes with the process.

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
processkit-cli inspect --run-id <id> --json
```

`inspect` finds the live runner for `<id>` through the registry, connects to its
endpoint, sends the `inspect` verb, and prints the snapshot as a single JSON line to
**stdout**. `--json` is required (it is the only supported output format today, and is
part of the fixed CLI form).

### The inspect snapshot

The snapshot is the machine-readable state of a live run. It is the control plane's
own client/runner contract, versioned on its own axis (`snapshot_version`), distinct
from the JSONL event `schema_version` and the `registry_version`.

| Field              | Type              | Notes                                                                 |
|--------------------|-------------------|-----------------------------------------------------------------------|
| `snapshot_version` | integer           | Snapshot format version (currently `1`).                              |
| `run_id`           | string            | The run's identifier — the key matched in the registry. Not a PID.    |
| `mechanism`        | string            | Containment mechanism: `job_object`, `cgroup_v2`, or `process_group` (same vocabulary as the JSONL `run_started`). |
| `root_pid`         | integer, nullable | The root child's PID; `null` if the backend exposed none.             |
| `started_at`       | string            | Run start time, RFC 3339 UTC, millisecond precision.                  |
| `members`          | array of member   | The container's members, **PID-only** — the scope the public `processkit` API exposes today, the same shape as the JSONL `members_snapshot` (the enriched `ppid`/`name`/`start_time` fields are present but `null`). Queried **at request time**, so it reflects the container's composition *when inspected*, not at start. |

Example:

```json
{"snapshot_version":1,"run_id":"build-42","mechanism":"job_object","root_pid":4242,"started_at":"2026-07-20T21:00:00.000Z","members":[{"pid":4242,"ppid":null,"name":null,"start_time":null}]}
```

## `cancel` and `kill`

```
processkit-cli cancel --run-id <id>
processkit-cli kill   --run-id <id>
```

Both find the live runner for `<id>` through the registry exactly as `inspect` does —
by matching `run_id`, **never** a PID — connect to its endpoint, send their verb, and
end the run. They differ only in *how* the run is ended:

- **`cancel`** asks the runner to run its **shared** soft-stop → grace → hard-kill
  teardown — the same path a `--timeout` or a `Ctrl-C` drives. On Unix a real
  `SIGTERM` is delivered to the tree, the `--grace` window (if the run was started
  with one) elapses, and the container's kill-on-drop then hard-tears-down whatever
  remains. On Windows there is no soft-terminate tier in the ProcessKit kernel yet, so
  — exactly as for a `Ctrl-C` — no soft signal is sent, the grace window still
  elapses, and the Job Object is then killed atomically. The run exits with the
  reserved **`CONTROL_CANCELLED` (108)**.
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
  apart from a `Ctrl-C`, which is `ctrl_c`), the `cleanup_started` / `cleanup_finished`
  teardown pair, and a terminal `runner_exit` with `source` `control_cancel` and code
  `108`.
- **`kill`** writes a dedicated `killed` event with `source` **`control_kill`**, the
  cleanup pair (with `soft_terminate` `null` — no soft stop was attempted), and a
  terminal `runner_exit` with `source` `control_kill` and code `109`.

See [`docs/schema.md`](schema.md) for these events.

### When the runner is gone: a distinguishable result, never a hang

Every client — `inspect`, `cancel`, and `kill` — can lose the runner the same two
ways. Both are reported as the reserved **`CONTROL` exit code (103)** — "could not
reach the target run" (see [`docs/exit-codes.md`](exit-codes.md)) — with an
explanatory message on **stderr** (naming the action and the run) and nothing on
stdout. Neither is a generic error, and neither hangs:

- **Stale registry entry.** The runner died abruptly, leaving its record behind; the
  released liveness lock makes the entry stale. The client detects this *before*
  connecting and reports the run as gone (its registry entry is stale).
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
