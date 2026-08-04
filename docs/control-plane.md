# Live-run control plane

The **control plane** lets a client query and (later) steer a *running*
`processkit-cli run`. It lives in the **live runner process**, not in named kernel
objects (`AGENTS.md`, "The control plane lives in the live runner process"): a runner
must stay alive to hold its kill-on-drop container, so the live process is exactly
where clients reach it. If the runner dies, the container tears the tree down and the
run becomes **detectably gone** — never a dangling handle a client could act on by
mistake.

This document is the normative description of the **local transport**, the **wire
protocol**, and the four clients — the read-only **`inspect`** and **`attest`**, and
the mutating **`cancel`** / **`kill`** — including their behavior when the runner
cannot be reached.
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
   are `inspect`, `cancel`, `kill`, and `attest`. (An empty line is also treated as
   `inspect`, so a bare connect-and-read probe still works.)
2. The server writes back **one JSON line** — the response — and closes the
   connection.

The responses per verb:

| Verb      | Response line                                | Effect on the run                                                        |
|-----------|----------------------------------------------|--------------------------------------------------------------------------|
| `inspect` | a [snapshot](#the-inspect-snapshot)          | none (read-only).                                                        |
| `cancel`  | an [ack](#cancel-and-kill) `{"accepted":true,"action":"cancel","run_id":"…"}` | the run runs its shared soft-stop → grace → hard-kill teardown and exits with `CONTROL_CANCELLED` (108). |
| `kill`    | an [ack](#cancel-and-kill) `{"accepted":true,"action":"kill","run_id":"…"}`   | the run hard-kills the whole tree immediately (no soft stop, no grace) and exits with `CONTROL_KILLED` (109). |
| `attest`  | an [attestation](#attest)                    | none (read-only).                                                        |

**No verb carries an argument, and for `attest` that is load-bearing.** The request
line is the verb and nothing else, so everything a response depends on is either the
run's own state or a property of the connection itself. `attest` answers about the
process on the other end of *this* connection, which the runner reads from the
transport rather than from the request — see below.

An unrecognized verb yields a JSON error object (`{"error":"..."}`) instead, and
changes nothing about the run — a foreign client cannot end a run by sending garbage.

For the mutating verbs the runner writes its **ack first**, then signals its own main
loop to tear down. The client therefore always receives its confirmation even though
the run ends the instant the signal lands; and if the ack cannot even be written (a
broken client), no teardown is signaled — an unconfirmed cancel never silently ends a
run.

## `inspect`

```
processkit-cli inspect (--run-id <id> | --all [--label <KEY=VALUE>]...) [--json]
```

`inspect` finds the live runner for `<id>` through the registry, connects to its
endpoint, sends the `inspect` verb, and prints the snapshot to **stdout** — as a
single JSON line with `--json`, or, by default, as a human-readable rendering
(snapshot version, run id, mechanism, root pid, start time, artifact locators, and a member table),
mirroring `list`/`prune`'s optional `--json`. `--json` is optional; `inspect --json`'s
output is unchanged from before `--json` became optional.

The aggregate form takes one snapshot of all confirmed-live registry records,
optionally filters them by repeated exact `--label KEY=VALUE` matches (logical AND),
and addresses each target by the record path and endpoint captured in that snapshot.
Without `--json`, it prints a terminal-safe table with one row per target and the
three-way status `inspected` / `already_gone` / `failed`, followed by a detailed
snapshot block for each inspected target. Those blocks reuse the single-run renderer,
including its member table and bounded handling of every untrusted string. An empty
matching fleet prints `no live runs to inspect`.

With `--json`, the original output remains one byte-compatible JSON array. Each
element has `run_id` and either a `snapshot` with `error: null`, or `snapshot: null`
with a bounded error string. If any target fails, either output form returns `CONTROL`
(103) after printing the complete report. This preserves partial fleet visibility
without turning registry churn into silent omission.

### The inspect snapshot

The snapshot is the machine-readable state of a live run. It is the control plane's
own client/runner contract, versioned on its own axis (`snapshot_version`), distinct
from the JSONL event `schema_version` and the `registry_version`.

| Field              | Type              | Notes                                                                 |
|--------------------|-------------------|-----------------------------------------------------------------------|
| `snapshot_version` | integer           | Snapshot format version this build writes (`2`), and the version the **runner** declared when reading. The client acts on it: a version newer than it implements is refused rather than rendered, an older one down to `1` is read — see "Snapshot version: a newer runner's reply is refused, an older one is read" below. |
| `run_id`           | string            | The run's identifier — the key matched in the registry. Not a PID.    |
| `mechanism`        | string            | Containment mechanism: `job_object`, `cgroup_v2`, or `process_group` (same vocabulary as the JSONL `run_started`). |
| `root_pid`         | integer, nullable | The root child's PID; `null` if the backend exposed none.             |
| `started_at`       | string            | Run start time, RFC 3339 UTC, millisecond precision.                  |
| `jsonl`            | string, nullable  | Absolute path to the JSONL lifecycle stream; `null` only when reading a version-1 snapshot, which had no such field. A runner of `snapshot_version` 2 always publishes a path — the nullability is what makes the older contract readable (below), not a caveat about this one. |
| `capture_dir`      | string, nullable  | Absolute capture directory, or `null` when capture is disabled.       |
| `members`          | array of member   | The container's members, enriched with `ppid`/executable `name`/`start_time` wherever ProcessKit's `members_info()` can report them — the same *member* shape as the JSONL `members_snapshot`'s own `members` array (`docs/schema.md`, "Enriched member fields"), and read through the same call, so the two views never drift. Only the member entries are shared: the JSONL event's own envelope fields (its `reason`, for instance) belong to that event, not to this reply. Fields stay `null` on platforms/members that can't report them (e.g. the "bare" BSDs). Queried **at request time**, so it reflects the container's composition *when inspected*, not at start. |

Example:

```json
{"snapshot_version":2,"run_id":"build-42","mechanism":"job_object","root_pid":4242,"started_at":"2026-07-20T21:00:00.000Z","jsonl":"C:\\runs\\build-42.jsonl","capture_dir":null,"members":[{"pid":4242,"ppid":4200,"name":"build.exe","start_time":"133456789000000000"}]}
```

### Snapshot version: a newer runner's reply is refused, an older one is read

`snapshot_version` is not decoration — the client **checks it and acts on it**. This
is the normative statement of that policy; the two `inspect` forms share one
implementation of it (`src/control/mod.rs`, `SnapshotReply::accept`).

**The rule.** This build reads a snapshot declaring version **1 or 2** — the range
from `MIN_READABLE_SNAPSHOT_VERSION` to `SNAPSHOT_VERSION` in `src/control/mod.rs` —
and **refuses** anything outside it with the reserved **`CONTROL` (103)** code and a
message naming the version that arrived, the range this build reads, and which way
the runner is out of that range (newer than this client, or older than anything it
still decodes). Nothing about a refused reply is printed: it never reaches the human
rendering or `--json`, and under `--all` that target is reported `failed` (with the
message in its `error` field), never `inspected` and never the successful
`already_gone` — the runner did not end, it answered something this client cannot
read. The verdict is taken from the declared number **before** the payload's shape is
parsed, so it holds even for a newer reply this build could not deserialize at all
(which is exactly the shape a breaking change produces).

**Why a newer version is refused.** A number above the one this build implements is
the runner's statement that the shape moved on in some way this build predates, and
this build cannot know which way: it holds no decoder for a contract written after it.
Rendering it anyway would present a payload interpreted under semantics its sender
never promised — and quietly, because the client re-serializes what it parsed, so a
newer runner's added fields are dropped at deserialization and never appear in the
output. The operator would see a confident rendering with no marker of what was lost.
This is the mixed-binaries case a mid-upgrade user really has (an older `inspect`
against a newer `run`), and the one this check exists for.

**Why an older version is not.** The refusal is deliberately one-sided. A lower number
does not, by itself, mean "unreadable": the only bump this contract has had — 1 → 2 —
was purely **additive** (it introduced `jsonl` and `capture_dir`, both optional with a
default, and changed no existing field), so this build decodes a version-1 snapshot
correctly, reporting those two as `null` — "not reported", which is precisely what a
version-1 runner meant. That is not a tolerance policy about numbers in general; it is
a checkable fact about this repository, pinned by a regression test, and it matters in
practice: every binary released so far (v0.1.0 … v0.3.1) writes version 1, so refusing
it would make an upgraded client unable to inspect the runs its own predecessor
started. When a future bump *does* make the older shape undecodable or misleading — a
removed, renamed, or retyped field, or an existing field whose meaning changed — the
floor (`MIN_READABLE_SNAPSHOT_VERSION`) moves up in that same change, and that is
where the judgement is recorded, rather than being inferred from the number.

This is a narrower refusal than the registry read side's, which skips a record whose
`registry_version` is not exactly its own ([`docs/registry.md`](registry.md)), and the
difference is earned: that check gates *destructive* action — probing a lock file and
reaping the record behind it — on liveness semantics an unknown version may have
redefined. A snapshot is read-only output whose only failure mode is being misread.

**What is printed.** The `snapshot_version` in a rendered snapshot is the value the
**runner** declared, unchanged — it reports which contract answered, so against an
older runner it is legitimately lower than the version this binary implements. The
rest of the object is this client's own re-serialization, so its field set is always
this build's. `fixtures/schema/cli/inspect.schema.json` therefore admits the readable
range on this field rather than pinning one value, and it moves when the range moves.

**What to do about a refusal.** Inspect that run with a `processkit-cli` build that
implements its snapshot version — for a newer runner, one at least as new as the
binary that started the run. Retrying the same command will not change the answer.
`probe --json` reports the `version` (and `probe_version`) of **the binary you run**,
which is how you tell two installed builds apart; it does not report a *runner's*
snapshot version, and no preflight can — that number arrives only in the runner's own
reply, which is what the refusal message quotes back to you.

**Consequence for a bump.** Bumping `SNAPSHOT_VERSION` is a real event for a mixed
deployment, not just a schema edit: every *older* client loses the ability to inspect
a runner that writes the new number — loudly, with `103`, rather than by
misinterpreting it. Newer clients keep reading older runners as long as the floor
allows, so a bump is not automatically a fleet-wide outage; deciding whether the floor
moves with it is part of making the bump, and both are announced in `CHANGELOG.md`.
`cancel`/`kill` are unaffected (their ack carries no version and is verified by
`accepted`/`action`/`run_id` instead), as are `list`, `wait`, and `prune`, which never
read a snapshot.

## `attest`

```
processkit-cli attest --run-id <id> [--json]
```

Asks the live run named `<id>` one question: **is the process running this command
inside your container?** The runner answers from the kernel's own record of who
opened the connection, so a positive answer is a containment fact rather than a claim
the caller repeated.

This is what an environment variable cannot be. An adapter that gates work on "the
caller belongs to run X" can otherwise only check the *shape* of a string the caller
carries — a convention enforced by instructions, not a checkable fact, since any
process can hold any string (and a string is inherited by processes that later leave
the container). `attest` turns that convention into an invariant the runner itself
verifies.

**The caller cannot name a process, and that is the whole design.** There is no
`--pid` and no equivalent. A caller-supplied pid would prove that *some chosen*
process is a member, which says nothing about the caller and would let any process
launder a membership claim about a pid it picked. The identity comes from the
transport:

- **Unix** — the socket's peer credentials. On Linux (and Android/OpenBSD) that is
  `getsockopt(SOL_SOCKET, SO_PEERCRED)`, whose `ucred` carries the peer's pid. On
  **macOS** `SO_PEERCRED` does not exist and the portable `getpeereid(3)` reports only
  the effective uid/gid — not an identity a membership check can use — so the pid
  comes from the Darwin-specific `getsockopt(SOL_LOCAL, LOCAL_PEEREPID)` instead;
  NetBSD uses `LOCAL_PEEREID`, and Solaris/illumos `getpeerucred(3C)`. The mechanism
  differs per platform; the guarantee — a pid the kernel attributes to the peer, not
  one the peer asserted — does not.
- **Windows** — `GetNamedPipeClientProcessId` on the connected pipe instance, which
  the object manager answers from its own record of the handle that opened it.

**PID reuse cannot produce a false positive.** The identity is read from the
connection while it is open, and the membership check runs on that same open
connection: a process holding an open socket or pipe handle has not exited, so its pid
cannot yet have been recycled onto a different process. An attestation is therefore a
statement about a peer that is demonstrably still there, which is also why it carries
`checked_at` — it is a point-in-time fact about a live connection, not a token to keep
and present later.

The pid is checked against the run's **own container membership**, through the very
same `members_info()` read that produces the `inspect` snapshot and the JSONL
`members_snapshot` (`docs/schema.md`, "Enriched member fields") — one notion of "a
container member" for the whole binary, never a second list assembled for this
command, and never a pid read back from the registry or any other file on disk.

The reply (`--json`; the default is the same fields rendered for a terminal):

```json
{"attestation_version":1,"run_id":"build-42","verdict":"member","peer_pid":4242,
 "mechanism":"job_object","checked_at":"2026-07-20T21:00:05.000Z"}
```

| Field | Meaning |
|---|---|
| `attestation_version` | The attestation contract's own version, currently `1`. Its own axis, independent of `snapshot_version` (see "Attestation version" below). |
| `run_id` | The run that answered, echoed and checked by the client, so a reply describing another run is refused rather than printed. |
| `verdict` | `member` \| `not_a_member` \| `peer_identity_unsupported` — see below. |
| `peer_pid` | The pid the kernel reported for the caller, as the runner sees it; `null` only for `peer_identity_unsupported`. Output, never input. |
| `mechanism` | The containment the verdict is about (`job_object` \| `cgroup_v2` \| `process_group`), which fixes its scope — see "What `member` means, per mechanism". |
| `checked_at` | When the runner decided, RFC 3339 UTC with millisecond precision. |

The three verdicts are three outcomes, deliberately not a boolean:

| Verdict | Exit code | `--error-format json` `kind` | Meaning |
|---|---|---|---|
| `member` | `0` | — | The caller is inside this run's container. |
| `not_a_member` | `NOT_A_MEMBER` (115) | `not_a_member` | The runner named the caller and it is **not** in the container. A decided answer. |
| `peer_identity_unsupported` | `CONTROL` (103) | `peer_identity_unsupported` | The runner could not obtain a kernel-authenticated identity for the caller, so it declined to answer either way. |

There is a fourth outcome, and it is deliberately **not** a verdict: if the runner
cannot read its *own* container membership at that moment (the `members_info()` query
itself fails), it produces no attestation at all. The client gets the same structured
error an unrecognized verb does, surfaces the runner's own words, and reports
`CONTROL` (103) — "the runner could not read its own container membership, so it
refused to decide". Answering `not_a_member` there would state a decided verdict, and
deny access through exit `115`, on the strength of a failed query; answering
`peer_identity_unsupported` would blame the wrong thing, since the caller *was* named.
This is the same honest-degradation discipline the JSONL `members_snapshot` event's
`read_error` flag follows — an `inspect` snapshot still degrades that failure to an
empty `members` array, because a diagnostic that reports nothing is still a
diagnostic, while a verdict that decides on nothing is not a verdict.

The attestation is printed on **stdout for every verdict**, including the two that
make the command exit non-zero — the same shape `probe --json` and `inspect --all
--json` already use when they report and then fail. The verdict is the answer the
caller asked for; the exit code says what to do about it, and under
`--error-format json` the matching envelope goes to stderr while stdout stays exactly
as it is (`fixtures/schema/cli/attest.schema.json`).

**Why a negative gets its own code.** Every `CONTROL` (103) result means *no answer
you can act on* — the target is missing, stale, unprobeable, ambiguous, unreachable,
too slow, or speaking a contract this build refuses. A `not_a_member` is the opposite:
the target was resolved, reached, and answered. An adapter that gated a lease on
membership must be able to tell "the runner says no" from "no runner said anything",
because the correct response differs (deny versus investigate or retry), so the two
never share a code. See [`docs/exit-codes.md`](exit-codes.md).

**Fail-closed on a platform that cannot answer.** A runner whose transport cannot name
its peer reports `peer_identity_unsupported` rather than degrading to an unproven
`member`. A consumer establishes that this cannot happen *before* it depends on
attestation, at preflight, with the capability token in `probe --json`'s `surface`:

```sh
processkit-cli probe --json --require-surface attest --require-surface attest:peer-identity
```

That token carries no `--` precisely because it is not a flag: it says this build can
obtain a kernel-authenticated peer identity **on this platform**, and it is absent
where the build cannot promise that. A missing capability is then the ordinary
fail-closed `PROBE_INCOMPATIBLE` (110) every other unmet `--require-*` produces, at
preflight, rather than a surprise in the middle of a job. The claim is deliberately
one-directional: its presence is a guarantee, its absence withholds one rather than
predicting failure (FreeBSD, for instance, supplies a peer pid on a new enough kernel
and not on an older one, which no compile-time claim can distinguish — so it is
excluded rather than over-claimed, and `attest` there still answers from whatever the
kernel really provides).

### What `member` means, per mechanism

`member` means exactly this: **the run's own container reports the caller as one of
its members.** Since that is the same enumeration `inspect` and `members_snapshot`
publish, what a `member` answer covers follows the mechanism the run obtained
(`run_started.mechanism`, and the `mechanism` field of the attestation itself):

- **`job_object` (Windows) and `cgroup_v2` (Linux)** enumerate the *whole contained
  tree*, so any process in the tree — a grandchild, a great-grandchild — attests as a
  member.
- **`process_group`** (the POSIX fallback: macOS and the non-FreeBSD BSDs, and Linux
  with no usable cgroup) *contains* a whole tree but *enumerates* only the tracked
  group leaders. Membership there is therefore decided against the caller's own
  **process group** — the predicate that mechanism actually enforces, and the one its
  teardown (`killpg`) reaches — so a contained grandchild attests as a member on this
  mechanism too, and a process that escaped the group with `setsid` (and so escaped
  this mechanism's containment) correctly does not.

**Nested runs.** A run started *inside* another run is an ordinary run: a client
inside it attests against it exactly as a client inside a top-level run does, on every
platform. Whether that client is *also* a member of the **outer** run is
mechanism-dependent, and the honest answer differs: Windows Job Objects nest, so the
inner run's processes are in the outer job as well and attest as members of it; a
Linux cgroup leaf is created inside the outer run's own cgroup, and a process moved
into that leaf is no longer listed in the outer cgroup's `cgroup.procs`, so it attests
as a non-member of the outer run even though the outer run's recursive teardown would
still reach it. Neither is a bug — each is a faithful report of what that mechanism
enumerates — so **ask about the run you actually mean**, and read `mechanism` if you
need to reason about the containment behind the answer.

### Attestation version

`attestation_version` is the contract's own axis, exactly as `snapshot_version` is
`inspect`'s, and the client **acts** on it: a reply declaring any other version is
refused with `CONTROL` (103) / `incompatible_contract` rather than rendered.

That is stricter than `inspect`'s range, on purpose rather than by omission. A misread
snapshot is a diagnostic shown under the wrong semantics; a misread attestation is a
security verdict, and an adapter would grant or deny access on a sentence its sender
never said. And strictness costs nothing here: this contract has had exactly one
version, so there is no older shape being refused — unlike `snapshot_version`, whose
floor records a checked fact about a bump that really happened (see "Snapshot version:
a newer runner's reply is refused, an older one is read"). If an additive bump ever
makes an older attestation genuinely readable, the range widens in the same change
that makes the widening true.

### The boundary: containment, not authentication

`attest` reports a **containment fact inside the existing same-OS-user threat model**
([`docs/threat-model.md`](threat-model.md)). It is **not** authentication between
mutually hostile peers, and must not be used as one:

- the control transport is owner-only, so every party to this exchange is already the
  same OS user, and that user's processes are inside the trust boundary this project
  draws — a same-user process that wanted to interfere with a run never needed to
  forge an attestation, since it can reach the control plane directly;
- what `attest` closes is the **forgeable correlation**: a process that is not
  contained claiming it is, by carrying a string. That is a real and common failure
  mode (a stale environment variable, a copied id, a process that outlived the run it
  was started for), and it is closed by asking the kernel instead of the caller;
- what it does **not** close is a same-user process that is genuinely inside the
  container behaving badly, nor anything about a *different* user (that is the
  transport's owner-only permissions, not this verb), nor any claim that survives the
  connection — the fact is scoped to the moment it was checked.

A consumer that needs a security boundary between mutually distrusting parties needs
OS-level isolation (separate users, containers, sandboxes); `attest` is a containment
invariant *within* one such boundary, not a replacement for one.

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

Every client — `inspect`, `cancel`, `kill`, and `attest` — can lose the runner the
same three ways (this applies per target under `--all` too, one snapshot entry at a
time). All
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

**Two refusals share this code without being a lost runner**, and both belong to the
read-only verbs. A runner
that answers with a snapshot declaring a `snapshot_version` outside the range this
client reads is reachable and perfectly healthy — the exchange completed and the reply
arrived — but that reply cannot be interpreted, so `inspect` refuses it with the same
`103` instead of rendering it (see "Snapshot version: a newer runner's reply is
refused, an older one is read"). `attest` has the same refusal for its own
`attestation_version`, plus one more of its own: a runner that could not obtain a
kernel-authenticated identity for the caller answers `peer_identity_unsupported`,
which is again `103` — reached, healthy, and unable to give an answer this client may
act on. That is what sets these apart from the reasons above:
they are all ways the *target* could not be resolved or reached, while here the target
answered and its **answer** was rejected or withheld. (Determinism is not the
distinguishing property — a confirmed-stale entry and an ambiguous `run_id` are just
as unaffected by a retry; only "died mid-conversation" is genuinely transient.)
`cancel`/`kill` cannot hit any of them — their ack carries no version and no identity.

**`attest`'s `not_a_member` is emphatically not in this family**, even though it is
also a non-zero exit from a control client: nothing was lost or unreachable, the
runner answered, and the answer is the point. It carries `NOT_A_MEMBER` (115) for
exactly that reason (see "`attest`").

This is the exit-code half of the contract: a caller distinguishes "here is the run's
state" / "the command was accepted" (exit `0`, JSON on stdout) from "that run is not
reachable" (exit `103`, message on stderr) without parsing free text.

Telling the reasons *within* that `103` apart is the one thing the code cannot do —
which is what the global `--error-format json` is for: under it the same failure
prints a bounded JSON object on stderr whose `kind` is exactly the distinction this
section draws (`stale`, `unprobed`, `control_unreachable`, `ipc_deadline`,
`ambiguous_run_id`, `not_found`, and — for the two refusals above —
`incompatible_contract` and `peer_identity_unsupported`). Still no free text parsed,
and stdout is untouched. See
[`docs/exit-codes.md`](exit-codes.md#machine-readable-failures---error-format-json).
