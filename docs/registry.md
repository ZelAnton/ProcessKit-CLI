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

`list` (see "Discovery" below), `prune` (see "Reaping" below), and `wait` (see
"Waiting" below) are the clients that read the registry directly, without connecting
to any runner's control transport: `list` scans every entry and prints it, so an
operator that has lost (or never had) a `run_id` can find one before reaching for
`inspect`/`cancel`/`kill`; `prune` reaps the entries `list` would show as stale; and
`wait` blocks on one entry until it is no longer live, so a supervisor that is not the
runner's parent can still wait for a run to end.

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
is re-asserted on every *mutating* open (`run`'s path, so a pre-existing directory
is locked down too before a record is written into it). A record names a run's
private control-channel endpoint, so a world-readable registry would hand that
channel to any local process. The **read-only** open every other client takes —
`list`, `prune`, `wait`, and the control clients — deliberately does neither: it does
not create the directory and does not touch its permissions, since a read-only scan
must not mutate registry state.

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
job — in `resolve_live_endpoint` (`src/control.rs`) for the control-plane verbs, and
in `Registry::probe_run` (`src/registry.rs`) for the registry-only `wait`, which
reaches the same verdict from its own scan — and it is deliberately conservative:

- The client scans every entry and filters to those matching the requested
  `run_id`, then counts how many of *those* are live (see "Staleness" above) —
  deliberately **before** ever looking at whether they publish an `endpoint`. A
  live entry that has not (yet, or ever) published an endpoint — a disconnected
  or failed transport — still counts as a live duplicate; if endpoint presence
  narrowed the count first, such an entry would be silently skipped and a
  duplicate could evade detection.
- **Zero** live matches → a distinguishable `CONTROL` (103) failure naming *why*
  (no such run registered at all, or the sole match is stale) — see
  [`docs/control-plane.md`](control-plane.md).
- **More than one** live match → also a `CONTROL` (103) failure, "ambiguous run
  id", instead of silently acting on whichever entry the directory scan happens
  to return first. This applies to **every** by-`run-id` client the same way — the
  destructive `cancel`/`kill` verbs, the read-only `inspect`, and the registry-only
  `wait` — rather than a softer fallback for the read-only ones: guessing wrong on a
  mutating verb ends the *other* run instead of the intended one, a snapshot of the
  wrong run under `inspect` is exactly as misleading as acting on it, and a `wait`
  that silently tracked one of two duplicates would report "your run finished" on the
  strength of the wrong run's ending. A caller that hits this is expected to pick a
  `--run-id` that is unique among currently live runs.
- **Exactly one** live match → only now does its endpoint matter: resolved
  normally if it published one, or a distinguishable `CONTROL` (103) failure
  ("the run is live but exposes no control endpoint") if it did not.

That single check happening once, at the start of the call, is a TOCTOU race for the
mutating verbs: `register` never enforces uniqueness, so a duplicate can register
under the same `run_id` in the window between the scan and the verb reaching the
runner over the transport (the connect round trip in between). `cancel`/`kill`
narrow that window as tightly as the registry's decentralized, no
locking-across-processes design allows: immediately before writing the verb, the
client re-runs the same scan+match and requires it to resolve back to the exact
endpoint it already connected to — any other outcome (a fresh ambiguity, the entry
having gone stale, or the resolution landing on a different entry) aborts the
command without ever writing to the wire. `inspect` does not repeat this check: being
read-only, a race that surfaces a snapshot from just before a duplicate registered is
merely stale information, not a wrong-target action.

That pre-dispatch re-check is a synchronous scan, while the verb write that follows
it is a separate, later `.await`; the two cannot be made atomic with each other, so a
duplicate can in principle still register in the sub-instruction gap between the
re-check returning and the write reaching the OS. Closing that residual gap
completely would need a `run_id`-keyed lock held across process boundaries through
the write — a registry redesign this resolver deliberately does not attempt (see "No
PID addressing" above). It is not needed for correctness, though: by the time the
re-check runs, the client has already connected to the target's specific,
uniquely-tokened transport endpoint (`endpoint_tokens_are_unique` in
`src/control.rs`), and a later registry write cannot retarget bytes already destined
for an open connection. So the guarantee the re-check actually buys is narrower than
"no ambiguity can ever exist at write time" (impossible without that cross-process
lock) and is instead: **the verb can never be misdirected to a different run than the
one already resolved and reconfirmed.** A duplicate that registers in the residual
gap is simply invisible to that call — it becomes visible on the *next* one — never a
wrong-target action. See
`racing_duplicate_after_reconfirm_does_not_misdirect_the_dispatched_verb` in
`src/control.rs` for a deterministic proof of this property.

## Discovery — `list`

`processkit-cli list [--json]` opens the registry through
[`Registry::open_read_only`] (`src/registry.rs`) — **not** the mutating
[`Registry::open`] `run` uses, so listing never creates the registry directory and
never touches its permissions — and scans it with [`Registry::entries`], the same
scan every other client shares, printing every entry it finds, live and stale alike:
`run_id`, health (`live`/`stale`), `started_at`, and `endpoint`. It is deliberately
**read-only** and never connects to any runner's control transport, so it carries
none of the "could not reach the target run" failure modes `inspect`/`cancel`/`kill`
do — it has no single target to fail to reach.

- **No `--json`** prints a human-readable table (or `no runs registered` for an
  empty registry).
- **`--json`** prints one JSON object per entry, one per line, sorted by `run_id`,
  then `started_at`, then the entry's registry record path (a tertiary tie-break,
  never itself printed) for a fully deterministic order even when two entries share
  both a `run_id` and a millisecond-precision `started_at` — the same "JSON Lines"
  shape `inspect --json` uses for a single snapshot.
- An **empty registry is not an error**: `list` prints an empty result (or the
  `no runs registered` notice) and exits `0`, exactly like scanning any other
  registry state.
- A **stale entry is listed, not hidden** — unlike `inspect`/`cancel`/`kill`, which
  treat a stale match as an unreachable-run failure, `list`'s whole purpose is
  discovery, so a stale leftover (evidence of a runner that died abruptly without
  cleaning up) is exactly the kind of thing an operator wants to see, e.g. before
  reaping it.
- A single corrupt or unreadable record is skipped by `Registry::entries` itself
  (see "Staleness" and the per-record degradation documented there) and never
  blinds `list` to the other, healthy entries — including a record whose
  `started_at` is not the well-formed `YYYY-MM-DDTHH:MM:SS.sssZ` shape a runner
  actually writes.

## Reaping — `prune`

`processkit-cli prune [--json]` is the cleanup counterpart to `list`. Where `list`
shows a stale leftover, `prune` deletes it: it opens the registry through
[`Registry::open_read_only`] (`src/registry.rs`) — like `list`, so it never creates
the directory or touches its permissions; a missing or empty registry simply has
nothing to prune — scans it with the same shared scan `list` uses, and for each
scanned record deletes **both** its files (`<stem>.json` then `<stem>.lock`, the same
order [`Registration::remove`] uses) only when it can *confirm* the record is stale.

It then makes a second pass over any **orphaned lock files** — a `.lock` with no
`.json` sibling at all. Such a `.lock` is invisible to the shared scan (which only
ever walks `.json` records), so however long it has sat there the paired-record pass
above can never find it. An orphan can still arise two ways: `Registry::register`
reserves and locks the `.lock` file *before* it writes the `.json` record, so a
`fs::write` failure in between would otherwise leave the fresh lock file behind
forever — this is now closed at the source by a Drop-backstop guard on the
reservation, armed until the record is published and disarmed right after, so a
failed `register` deletes its own lock file instead of leaking it; or
`Registration::remove`'s best-effort `.json` delete can succeed while its `.lock`
delete does not, which is not similarly guarded. The second pass reuses the exact
same lock-probe safety as the first (a `Live` lock is never touched, a probe failure
leaves the file in place, only a confirmed-stale lock is deleted) and reports its
reaps under the separate `orphaned_locks` tally below — kept distinct from `pruned`
because it deletes one file, not a `.json`/`.lock` pair.

A record-less lock file needs one more guard the paired-record pass does not: a
freshly `create_new`-d `.lock` file that a **just-starting** `Registry::register` has
not yet locked is, for a moment, indistinguishable from a genuine, long-dead orphan —
both are simply an unlocked `.lock` with no `.json` next to them. The second pass
therefore only ever considers a candidate whose mtime is already at least a few
seconds old (`ORPHAN_LOCK_MIN_AGE` in `src/registry.rs`); a genuine orphan never ages
out of that check, so the extra latency costs nothing, while the two-syscall
reservation window reliably falls inside it. `Registry::reserve_entry` closes the
same window from its own side: after taking its lock it re-checks that `lock_path`
still resolves to the identical file it is holding open (device/inode on Unix, file
index + volume serial number on Windows) before trusting it enough to publish a
record naming it, and retries with a fresh stem — never a hard error — both when the
lock is denied and when that identity check fails. Together these close the race a
lock-probe-only guarantee would otherwise leave open (see "The reaping safety
invariant" below).

### The reaping safety invariant

Pruning deletes files, so it is deliberately conservative: an entry is reaped **only**
when its own liveness probe *succeeds and reports stale*. The three probe outcomes are
kept strictly apart — and this is the load-bearing distinction:

- **Confirmed stale ⇒ reaped.** The lock file is absent (stale by definition), or its
  exclusive lock was free and the probe took it (no live runner holds it). Only this
  case deletes anything.
- **Live ⇒ never touched.** A live runner holds the lock, so its entry is left exactly
  as it is. Prune never deletes a running run's record.
- **Probe failed ⇒ left in place.** The probe could not even be performed — the lock
  file would not open (a directory in its place, a permission error, a rejected
  symlink/reparse point) or the lock call itself errored. Liveness is *unknown*, not
  confirmed stale, so the entry is **kept**, on every repeated prune. This is the case
  the `list`/`inspect` read path deliberately collapses into `stale` (its
  "could not confirm liveness ⇒ treat as not live" degradation): prune must **not**
  reuse that collapsed verdict — a probe-failed record is not a confirmed-dead one —
  so it probes on its own path that keeps the failure distinct, and errs toward
  keeping a record it is unsure about.

Two further guarantees hold, mirroring the rest of the registry:

- **Never by PID.** A reaped entry is addressed only through the record path the
  directory scan produced (the same PID-free tokened stem — see "No PID addressing"
  above), never by a process id, so PID reuse can never misdirect a deletion.
- **Reclaim under the lock.** A confirmed-stale entry is deleted **while the probe
  still holds its exclusive lock** — the "keep the lock to reclaim" behavior noted
  under "Staleness" above — so a second concurrent prune sees the entry as live and
  skips it instead of racing on the same files.

A `.lock` file with no `.json` sibling carries one further precondition **before**
any of the three probe outcomes above even apply: it must already be at least
`ORPHAN_LOCK_MIN_AGE` old (by mtime). A confirmed-stale-*or*-live-*or*-unprobed
verdict on a lock-probe alone is not enough to call a record-less lock file safe to
touch, because a freshly reserved-but-not-yet-locked file would otherwise read as
"probe succeeded, no live holder" — indistinguishable from a genuine orphan purely by
scheduling luck. A too-young candidate (or one whose age could not be confirmed at
all) is simply left alone this round, to be reconsidered once it has aged.

Corrupt records the scan already skips (unreadable, unparsable JSON, a malformed
`started_at`, or a `lock_file` that is not a simple in-directory name) are **not**
prune candidates: they are never probed and never deleted, exactly as `list` leaves
them alone — and a `.lock` file that *does* have a `.json` sibling, however corrupt,
is likewise left to that first pass (or to neither pass, for a corrupt record),
never treated as orphaned. Every deletion is best-effort and per-entry — an OS error
reaping one entry never aborts the reaping of the others (the leftover just reads as
stale again next time) — and pruning an already-clean, empty, or missing registry is
a no-op that exits `0`.

- **No `--json`** prints a one-line summary (`no stale entries to prune` when there
  was nothing to reap).
- **`--json`** prints a single JSON object with the tally: `pruned` (paired entries
  reaped), `live` (live entries left untouched, paired or orphaned lock alike),
  `unprobed` (entries whose probe failed and were left in place, paired or orphaned
  lock alike), and `orphaned_locks` (lone `.lock` files with no `.json` sibling that
  were reaped).

## Waiting — `wait`

`processkit-cli wait --run-id <id> [--timeout <duration>]` is the *lifetime*
counterpart to `list`'s discovery and `prune`'s cleanup: it blocks while the named run
is live and returns as soon as it is not. It exists for the supervisor that did **not**
start the run — an adapter that restarted, a cleanup step, anything holding only a
`run_id` — and therefore has no child process to wait on. Like `list` and `prune` it
opens the registry through `Registry::open_read_only` (`src/registry.rs`), so waiting
never creates the registry directory or touches its permissions, and unlike
`inspect`/`cancel`/`kill` it never connects to the run's control transport: the run is
not disturbed, not ended, and not even aware of the waiter. A run whose transport never
came up (a `null` `endpoint`, see "Record format" above) is still perfectly waitable —
`wait` needs no endpoint.

A run started with `run --detach` is the case this was written for: that call returns
once the run has started and is *never* the runner's parent, so `wait` (plus the run's
`--jsonl` stream for the outcome itself) is how its caller learns the run is over. A
detached run publishes an ordinary record here — nothing about the entry, its liveness
lock, or its removal on a clean exit differs — so `list`, `prune`, and `wait` treat it
exactly like any other.

**How it waits.** Liveness is the advisory lock described under "Staleness" above, and
that lock offers no event, notification, or wakeup a third process could subscribe to.
Waiting on it is therefore honest *periodic probing* — one scan plus one non-blocking
lock attempt per matching record, a few times a second (`POLL_INTERVAL` in
`src/wait.rs`) — not an event subscription dressed up as one. Blocking on the lock
itself would be worse than merely slow-to-notice: acquiring a stale entry's lock is how
a *reclaimer* claims it (see "Reaping" above), and it would still miss the ordinary
clean exit, which deletes both files rather than handing the lock over.

**Three outcomes, by exit code:**

- **`0`** — the run is over: no record matches the `run_id` any more, or every record
  that does probed as stale.
- **`WAIT_TIMEOUT` (112)** — the `--timeout` given to `wait` elapsed while the run was
  still live. This is *the waiter's* deadline, not the run's: the run was left running,
  untouched, and will report its own ending in its own time. It is deliberately not the
  run's `TIMEOUT` (106), which means the opposite — see
  [`docs/exit-codes.md`](exit-codes.md), "A waiter's deadline is not a run's deadline".
  Without `--timeout`, `wait` blocks indefinitely.
- **`CONTROL` (103)** — the `run_id` is ambiguous: more than one live entry matches, so
  there is no single run whose end could be waited for (see "Run id resolution" above).
  Re-checked on every probe, not just the first, since a duplicate can register at any
  moment.

Nothing is printed on success — the exit code is the whole answer, so `wait` has no
output format to keep stable and no `--json` to add one. A registry that cannot be
opened or read at all is a `SETUP` (111) failure, exactly as it is for `list`/`prune`.

### An unknown `run_id` reads as "finished"

A run that exits cleanly **deletes its own registry entry** (see "Lifecycle" below), and
the registry keeps no history of what used to be there. So "`build-42` was never
registered" and "`build-42` finished a moment before you asked" are the *same
observation*: no matching record. Nothing in the registry can separate them.

`wait` therefore answers both the same way — exit `0`, "it is not running" — rather than
inventing a third outcome that could only ever be a guess. Failing on an unknown id
would be worse than unhelpful: it would make the result depend on *when* the caller
asked, turning the ordinary, expected race (the run finished while the adapter was
starting up) into a hard error — precisely the failure mode a `wait` command exists to
remove.

The consequence a caller must plan for is the mirror image: **a mistyped `run_id`
returns `0` immediately**, indistinguishable from a fast, successful run. `wait`'s `0`
means "not running", never "existed and completed". A caller that needs the stronger
fact must establish it separately — it launched the run itself, or it saw the id in
`list` — and must not read a `0` as proof the run ever existed.

### Liveness it cannot confirm

One case is neither live nor confirmed over: a matching record whose lock file cannot be
probed at all (a directory in its place, a permission error, a rejected reparse point —
the same "probe failed" case "The reaping safety invariant" above keeps apart from
"confirmed stale"). `Registry::entries` collapses that failure into `stale`, which is
right for `list`/`inspect`, whose worst case is showing or refusing; it is wrong for
`wait`, whose `0` is a positive claim about a run's lifetime.

So `wait` probes on its own path and **keeps waiting** on an unconfirmable entry rather
than announcing a completion it never observed. A bounded caller still gets a definite
answer when `--timeout` elapses (`WAIT_TIMEOUT`, honestly meaning "could not confirm
completion in time"); an unbounded one keeps waiting for a real verdict. This is the
same "unknown is not confirmed" stance `prune` takes when it refuses to reap an entry it
could not probe.

## Lifecycle

- **Create.** `run` writes the record and takes the liveness lock **before** the
  child is spawned, so the entry exists for the whole run. Creating the registry is
  **best-effort**: if it fails, the runner warns on stderr and proceeds — the
  registry is control-plane *discovery* infrastructure, and losing it must never cost
  the child its faithfully forwarded exit code (`AGENTS.md`, "Exit-code fidelity").
- **Remove.** On a clean exit the entry is removed from the **same teardown site as
  the container reap** in `src/run.rs`, on **every decided ending** — a normal child
  exit, a `--timeout`, a local stop-signal cancel (`Ctrl-C`, on Unix a
  `SIGTERM`/`SIGHUP`, or on Windows a `Ctrl-Break`/console close/logoff/system
  shutdown, all of which the runner catches), or a control-plane
  `cancel`/`kill` — not just the happy path.
- **Leak → stale.** An abrupt death skips that removal by definition, leaving the
  record on disk. The released lock makes it detectably stale, per the section above.
  This is genuinely abrupt death only — a crash, a `SIGKILL`, an outer Job Object
  terminate — not an ordinary Unix `SIGTERM`/`SIGHUP` or a caught Windows console-
  control event (`Ctrl-Break`/console close/logoff/system shutdown), all of which the
  runner catches and turns into the clean removal above.
