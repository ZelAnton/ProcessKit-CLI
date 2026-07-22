# Run registry

The **run registry** is the first brick of processkit-cli's control plane. The
control plane lives in the live `run` process, not in named kernel objects
(`AGENTS.md`, "The control plane lives in the live runner process"): a runner must
stay alive to hold its kill-on-drop container, so the live process is exactly where
`inspect` / `cancel` / `kill` (later tasks) reach it. The registry is how those
clients *find* a live runner — a per-user directory holding one record per in-flight
run.

This document is the normative description of the registry's **location**, **record
format**, and **staleness signal**. The transport those clients speak over, and the
`inspect` client itself, are described in [`docs/control-plane.md`](control-plane.md);
here we define only the registry.

## Location

The registry is a **per-user** directory — not system-wide and not tied to any one
run. It is resolved in this order:

1. **`PROCESSKIT_CLI_REGISTRY_DIR`** — if set (and non-empty), it is used verbatim as
   the registry directory. This lets an orchestrator pin the location and lets the
   tests isolate a scratch registry.
2. **Platform default**, otherwise:
   - **Unix:** `$XDG_RUNTIME_DIR/processkit-cli/runs` when `XDG_RUNTIME_DIR` is set —
     a user-private, per-session runtime directory is the natural home for live-run
     state — else `$HOME/.local/state/processkit-cli/runs`.
   - **Windows:** `%LOCALAPPDATA%\processkit-cli\runs`, falling back to the same path
     built from `%USERPROFILE%`.

### Permissions

The registry directory is created **restricted to its owner**, and the restriction
is re-asserted on every open (so a pre-existing directory is locked down too). A
record names a run's private control-channel endpoint, so a world-readable registry
would hand that channel to any local process.

- **Unix:** mode `0700`. Applied at creation and re-asserted with `chmod` (which,
  unlike the creating `mkdir`, is not filtered by the umask).
- **Windows:** a **protected** DACL that grants full control only to the current
  user — the equivalent of `0700`. Concretely the directory's DACL is replaced with
  `D:P(A;OICI;FA;;;<current-user-SID>)`: **P**rotected (inherited ACEs from the
  parent are blocked), a single allow-**F**ull-**A**ccess ACE for the current user,
  inherited by child objects and containers (**OICI**).

## Record format

Each run writes one **record file** (`<opaque-stem>.json`) plus a sibling **lock
file** (`<opaque-stem>.lock`). The record is a single JSON object:

```json
{
  "registry_version": 1,
  "run_id": "run-1234-...",
  "endpoint": "\\\\.\\pipe\\processkit-cli-1234-...",
  "started_at": "2026-07-20T21:00:00.000Z",
  "liveness": {
    "kind": "advisory_lock",
    "lock_file": "run-000...-0000.lock"
  }
}
```

| Field              | Meaning |
| ------------------ | ------- |
| `registry_version` | Record format version (currently `1`). Independent of the JSONL event `schema_version` — the registry is a private per-user contract, not the public event stream, so it versions on its own axis. |
| `run_id`           | The run's identifier (`--run-id`, or a generated one). **This is the key** clients match on. |
| `endpoint`         | The run's local control-transport connection address — a unix socket path, or a Windows named-pipe name (see [`docs/control-plane.md`](control-plane.md)). A live runner publishes it here so a client can reach it; `null` only when the transport could not be stood up (best-effort degradation — the run still works, it is just not inspectable). |
| `started_at`       | Run start time, RFC 3339 UTC with millisecond precision. |
| `liveness`         | How to decide whether the record is live or stale (see below). |

### No PID addressing

A record is **never indexed or identified by a bare PID** (`AGENTS.md`: "Nothing is
addressed by PID"). The file name (`<opaque-stem>`) is a PID-free time+counter token
whose only job is to be unique; the authoritative identity is the `run_id` field.
Clients find a run by scanning records and matching `run_id`, so **PID reuse cannot
alias one run onto another**. Uniqueness of the file name is guaranteed by the
filesystem (the lock file is created with `O_EXCL` / `CREATE_NEW`), so concurrent
runs always get independent entries that neither overwrite nor block each other.

## Staleness — detectable, and not by file existence

If a runner dies abruptly (crash, `SIGKILL`, a parent's Job Object terminate), the
kernel container reaps the whole process tree, but the record file is **left
behind**. A client must be able to tell that leftover record from a live one — and
crucially, **the file merely existing is not enough** to conclude the run is alive.

The signal is an **OS advisory lock**:

- A live runner holds an **exclusive advisory lock** on the record's lock file for
  the entire run (`flock(LOCK_EX)` on unix; `LockFileEx` with
  `LOCKFILE_EXCLUSIVE_LOCK` on Windows). The lock is tied to the open file handle,
  and the operating system **releases it automatically when the process dies** — by
  any means, clean or abrupt.
- A client checks liveness by **trying to take that same lock**, non-blocking:
  - **Denied** (the lock is held) → a live runner owns it → the entry is **live**.
  - **Acquired** (no one holds it) → the runner is gone → the entry is **stale**.
  - **Lock file missing** → **stale** by definition.

Because the verdict comes from the lock — which the OS frees on death — and not from
the file's presence, an orphaned record is reliably classified as stale. A client
performing a pure liveness *query* releases the lock immediately after acquiring it;
a client that intends to *reclaim* a stale entry would instead keep the lock held to
claim it atomically.

## Run id resolution — ambiguity is a hard failure

The registry does **not** enforce uniqueness of `run_id` at `register` time: two
concurrent runs started with the same explicit `--run-id` are both written as
independent entries (independent opaque file stems — see "No PID addressing" above)
and both read as live for as long as they run. Resolution is therefore the client's
job, in `resolve_live_endpoint` (`src/control.rs`), and it is deliberately
conservative:

- The client scans every entry, filters to those matching the requested `run_id`,
  and narrows to the ones that are both live (see "Staleness" above) and publish an
  `endpoint`.
- **Exactly one** such entry → resolved, normally.
- **Zero** → a distinguishable `CONTROL` (103) failure naming *why* (no such run
  registered at all, the sole match is stale, or the sole live match predates the
  transport) — see [`docs/control-plane.md`](control-plane.md).
- **More than one** → also a `CONTROL` (103) failure, "ambiguous run id", instead of
  silently acting on whichever entry the directory scan happens to return first.
  This applies to **every** client the same way — the destructive `cancel`/`kill`
  verbs *and* the read-only `inspect` — rather than a softer fallback for `inspect`:
  guessing wrong on a mutating verb ends the *other* run instead of the intended
  one, and a snapshot of the wrong run under `inspect` is exactly as misleading as
  acting on it. A caller that hits this is expected to pick a `--run-id` that is
  unique among currently live runs.

That single check happening once, at the start of the call, is a TOCTOU race for the
mutating verbs: `register` never enforces uniqueness, so a duplicate can register
under the same `run_id` in the window between the scan and the verb reaching the
runner over the transport (the connect round trip in between). `cancel`/`kill`
narrow that window as tightly as the registry's decentralized, no
locking-across-processes design allows: immediately before writing the verb, the
client re-runs the same scan+match and requires it to resolve back to the exact
endpoint it already connected to — any other outcome (a fresh ambiguity, the entry
having gone stale, or the resolution landing on a different entry) aborts the
command without ever writing to the wire, rather than letting it silently proceed
against a target that is now ambiguous. `inspect` does not repeat this check: being
read-only, a race that surfaces a snapshot from just before a duplicate registered is
merely stale information, not a wrong-target action.

## Lifecycle

- **Create.** `run` writes the record and takes the liveness lock **before** the
  child is spawned, so the entry exists for the whole run. Creating the registry is
  **best-effort**: if it fails, the runner warns on stderr and proceeds — the
  registry is control-plane *discovery* infrastructure, and losing it must never cost
  the child its faithfully forwarded exit code (`AGENTS.md`, "Exit-code fidelity").
- **Remove.** On a clean exit the entry is removed from the **same teardown site as
  the container reap** in `src/run.rs`, on **every decided ending** — a normal child
  exit, a `--timeout`, or a `Ctrl-C` cancel — not just the happy path.
- **Leak → stale.** An abrupt death skips that removal by definition, leaving the
  record on disk. The released lock makes it detectably stale, per the section above.
