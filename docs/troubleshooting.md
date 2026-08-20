# Troubleshooting

This is an **operator's** guide: symptom, what to look at, and where the
normative answer lives. It does not restate the normative documents —
[`docs/schema.md`](schema.md), [`docs/exit-codes.md`](exit-codes.md),
[`docs/registry.md`](registry.md), and [`docs/control-plane.md`](control-plane.md)
— it points at them. For the consumer/adapter walkthrough (preflight,
launching, reading the stream, supervision, housekeeping), see
[`docs/integration.md`](integration.md) instead; this document is organized by
symptom rather than by call sequence, and each entry below is deliberately
short. On any disagreement between this document and one of the normative
ones, the normative document is the source of truth.

## Qualifying a host: `doctor`

**Symptom.** `probe` says the binary is compatible, but the first real `run` on this
machine fails — or runs work on one host and not on another, and you want to know
*which* part of the environment differs before spending a production run finding out.

**Diagnose.** Run the host qualification:

```sh
processkit-cli doctor          # human-readable
processkit-cli doctor --json   # one JSON line, for an adapter
```

`doctor` is the side-effecting counterpart of `probe`, and the difference is the whole
point of having both. `probe` reads compile-time constants and the in-memory CLI tree:
it proves *this binary* exposes the surface you need, spawns nothing, and touches no
registry, container, or transport. `doctor` proves *this host* can actually run a
contained process, by doing it: it performs a bounded scratch run of this binary's own
harmless child (`doctor --scratch-child`, which sleeps briefly and does nothing else),
drives that run as an ordinary control-plane client, and reports what it observed. A
passing `probe` and a failing `doctor` is the normal shape of an environment problem.

The report is a list of facts, never one boolean:

| Fact | What it tells you |
| --- | --- |
| `registry.dir`, `registry.owner_only`, `registry.protection` | Which per-user registry directory this host resolves to, and whether it really is protected to its owner alone — re-read from the filesystem, not assumed from the create having succeeded ([`docs/registry.md`](registry.md)). |
| `containment.mechanism`, `containment.abrupt_cleanup` | Which containment mechanism this host actually gave the run, and what still reaps the tree if the runner is killed outright. The same values the `run_started` event publishes ([`docs/schema.md`](schema.md#run_started)) — so a `cgroup_v2` → `process_group` fallback (above) is visible here at setup time instead of mid-incident. |
| `control.*` | That the local transport bound, answered an `inspect` with real members, accepted a `cancel`, and that the run ended *because of that cancel* (`terminal_exit_code: 108`, `terminal_source: "control_cancel"`). |
| `cleanup.*` | That teardown was **confirmed** empty rather than assumed — `kill_error` says whether the hard kill succeeded and `read_error` whether the member read succeeded — and that every artifact is gone: the registry record, the control endpoint, the scratch files. |
| `resource_controller` | Only with `--check-resource-controller`: whether a whole-tree cap (`run --max-processes`) can be installed here at all. `available: false` is written only on the scratch run's own `limit_hit` evidence that the cap was refused (the entry below), so `null` always means *nothing was established* — the check was not asked for, or it ran and could not reach a verdict, in which case its phase fails and says why — and never *not available*. |
| `phases[].elapsed_ms` | Where the time went. A host that is merely slow is diagnosable as such instead of arriving as a generic hang. |

**Reading a negative verdict.** `doctor` exits `HOST_UNQUALIFIED` (116) when a phase
failed or a `--require-*` expectation was not met, and prints the report either way
(see [`docs/exit-codes.md`](exit-codes.md), "Qualifying a host: `doctor`"). The two
cases are distinguishable in the report itself: `failures` names phases that did not
work, `mismatches` names host properties that are not what you asked for. **A failed
phase keeps its evidence**: the scratch directory is not deleted, and
`diagnostics_dir` names it — it holds the scratch run's own JSONL stream, the runner's
stdout/stderr, and a copy of the report. A qualified host leaves nothing behind at all.

**Pinning what you need.** The requirement flags gate the exit code and nothing else —
the observed facts are reported identically with or without them:

```sh
processkit-cli doctor --json --require-abrupt-cleanup whole_tree
processkit-cli doctor --json --require-mechanism cgroup_v2
processkit-cli doctor --json --check-resource-controller --require-resource-controller
```

Each compares for **exact equality** against the value this host reports, and names
both sides on a mismatch. `--require-abrupt-cleanup` in particular is not an "at least
this strong" comparison: the three levels are platform facts
([`docs/platform-support.md`](platform-support.md)), and this project publishes no
ordering between them to compare against.

**What it costs, and what it touches.** One (or, with
`--check-resource-controller`, two) short scratch runs of this binary against itself,
in the real per-user registry the host uses — which is the point: a qualification of an
isolated sandbox would qualify the sandbox. The whole check is bounded by `--timeout`
(default `30s`), and the scratch run and its child carry that bound themselves, so a
`doctor` that is itself killed leaves nothing running behind it.

## `BACKEND` (102) with a `limit_hit` event, often only on CI or under systemd

**Symptom.** `run --max-memory <size>` / `--max-processes <n>` / `--cpu-quota
<cores>` exits `BACKEND` (`102`) immediately — no child output at all — even
though the same command works locally without the flag, or works locally
*with* it.

**Diagnose.** Read the `--jsonl` stream: a `limit_hit` event (naming which
limit — `memory` / `processes` / `cpu` — in its `limit` field) precedes the
`container_failed` (`phase: "create"`) and terminal `runner_exit`
(`source: "container_error"`, `code: 102`). The `limit_hit` event, not the
exit code, is what tells you this specific ending was a resource cap the
platform could not apply — see [`docs/schema.md`](schema.md#limit_hit).

**Why it happens.** A whole-tree cap needs a real container. On Linux that
means cgroup v2 **at the real hierarchy root** — a minimal, non-systemd init.
It does **not** work under a systemd session/scope/service, inside an
ordinary container (Docker/Kubernetes), or under typical hosted CI (including
GitHub Actions' `ubuntu-latest`), because the controllers cannot be enabled
there; the run fails fast rather than silently running unbounded. FreeBSD's
process reaper still provides whole-tree normal containment, membership, and
kill/teardown semantics, but it has no memory, CPU, or process-count cap support,
so any cap request fails there too. macOS and non-FreeBSD BSD process groups,
along with the Linux process-group fallback, have no whole-tree cap container at
all, so any cap request fails the same way there too. See the
[resource-limit platform matrix](resource-limits.md#platform-matrix) for the
separate FreeBSD process-reaper row and the distinct macOS/non-FreeBSD BSD
process-group row, and
[`docs/exit-codes.md`](exit-codes.md#resource-limits-reuse-backend-102) for
why this reuses `BACKEND` (102) instead of a dedicated code.

**Fix.** Either run somewhere the cap can actually be enforced (a Windows Job
Object, or a real Linux cgroup v2 root), or drop the resource-limit flags —
there is no partial/best-effort mode.

## The honest fallback: `cgroup_v2` → `process_group`

**Symptom.** On Linux you expected cgroup v2 containment (whole-tree teardown
and process accounting) but observe process-group-only behavior instead — for
example a descendant that left the process group via `setsid`/double-fork
surviving an ordinary teardown, or a just-exited child still listed briefly in
a post-kill member snapshot.

**Diagnose.** The `run_started` event's `mechanism` field (also echoed live by
`inspect --json`'s snapshot) reports which containment mechanism this
specific run actually got — `cgroup_v2` or `process_group` — never a promise
based on the platform alone; that field alone tells you whether the fallback
happened. Do not use `abrupt_cleanup` (also on `run_started`) to tell the two
apart: it is a separate, OS-derived contract — `whole_tree` on Windows,
`direct_child_only` on Linux, `none` on macOS/other Unix — sourced from the
platform's parent-death-signal capability, not from which mechanism this run
got. On Linux it reads `direct_child_only` whether the run got `cgroup_v2` or
fell back to `process_group`, so comparing it against `mechanism` tells you
nothing about the fallback. See [`docs/schema.md`](schema.md#run_started) and
[`docs/control-plane.md`](control-plane.md#the-inspect-snapshot).

**Why it happens.** Where cgroup v2 delegation is unavailable to the runner,
it falls back to the POSIX process-group mechanism rather than claiming a
cgroup it did not get — the same unavailability this document's first entry
covers for resource limits, but here it is a silent, successful fallback
instead of a hard failure, because plain containment (unlike a *requested*
cap) has a working fallback. What the fallback actually costs is ordinary
teardown/accounting strength, not extra abrupt-death coverage: if the runner
itself dies abruptly, a cgroup does not automatically kill grandchildren
either — only the direct child is covered, by the parent-death signal, under
either mechanism. See `README.md`, "Platform matrix", for the per-mechanism
guarantees.

## A console window pops up for a detached run

**Symptom.** `run --detach` launches a console-based child on Windows and a
new, unwanted console window appears (or flashes) even though nothing about
the invocation looks interactive.

**Diagnose.** No JSONL event is involved — this is purely an OS behavior:
Windows gives a console-allocating child a fresh console of its own whenever
its parent has none. The detached runner itself has no console (it was
launched with `DETACHED_PROCESS`), so any console-based child it starts gets
one unless told not to.

**Fix.** Pass `--create-no-window` alongside `--detach` — it maps directly
onto ProcessKit's `Command::create_no_window()` (the `CREATE_NO_WINDOW`
creation flag; a no-op on non-Windows platforms). It defaults to *off* for an
ordinary foreground `run` (so a bare `run` still behaves like a direct
launch), but a detached run is exactly the case where passing it matters
most. See `README.md`, "Windows console", and `README.md`, "Detached runs".

## `list` shows an entry as `unprobed`

**Symptom.** `list`/`list --json` shows a registry entry's health as
`unprobed` rather than `live` or `stale`, and you are not sure whether it is
safe to delete by hand; or `prune --json`'s tally keeps reporting a non-zero
`unprobed` count across repeated runs instead of reaping those entries.

**Diagnose.** `list`'s health field has three values, matching the same
tri-state verdict `prune`/`wait` already use internally: `"live"`, `"stale"`
(confirmed dead — the liveness lock probed as released), and `"unprobed"` (the
liveness lock genuinely could not be probed at all: the lock file would not
open — a directory in its place, a permission error, a rejected reparse
point — or the lock call itself errored). `"unprobed"` is a deliberately
distinct, conservative verdict — "could not confirm liveness" is not the same
claim as "confirmed dead" — and `prune` (and its non-destructive
`prune --dry-run` preview) never reap an entry in this state, on every
repeated run, until the probe itself can succeed. A control client
(`inspect`/`cancel`/`kill`/`attest`) aimed at such an entry refuses with `CONTROL`
(103), since it acts only on a **confirmed-live** entry — but its message,
too, reports that liveness could not be probed rather than that the runner is
gone (see the `CONTROL` (103) entry below).

A non-zero `unprobed` count in `prune --json`/`prune --dry-run --json` is not
always the same set of things `list` shows you as `unprobed`, though: the
tally is shared between this per-entry probe (one `.json`/`.lock` pair, the
same one `list` reports on) and a second, independent pass over **orphaned
`.lock` files** — a `.lock` with no `.json` sibling at all, invisible to
`list`, which only ever walks `.json` records. So the count can include lock
files `list` has no entry for at all, on top of any `unprobed` entries `list`
already showed you. See [`docs/registry.md`](registry.md) ("Discovery" for
what `list` reports) and
[`docs/registry.md`](registry.md#the-reaping-safety-invariant) for exactly
which of the three probe outcomes `prune` reaps.

**Fix.** Run `prune --dry-run --json` first to see precisely what a real
`prune` would reap (and what it would leave as `unprobed`) before running the
destructive form. For an `unprobed` entry `list` already shows you, or for any
excess the dry-run's tally reports beyond that, investigate the registry
directory and its `.lock` files directly (the usual cause is a permissions
issue or a path collision) rather than deleting registry files by hand.

## `CONTROL` (103): the runner could not be reached

**Symptom.** `inspect` / `cancel` / `kill` / `attest` exits `103` and prints an
explanatory line on stderr. For the by-`run_id` form this means the command did
nothing to any run; **`cancel --all` / `kill --all` are the exception** — see
"`cancel --all` / `kill --all` and a partial `103`" below before assuming
nothing happened.

**Diagnose.** stderr names which reason applied. Most of them mean the runner
itself was never reached: a `run_id` the registry names nowhere (`not_found`) or
names more than once ("An ambiguous `run_id`" below), plus the three described
here, where an entry was found but no answer came back. The read-only verbs then
add two refusals of their own below, where the target *was* reached and did
answer. If you are diagnosing this from a script rather than by eye, re-run the
same command with `--error-format json`, which prints the reason as a
machine-readable `kind` (`stale` / `unprobed` / `control_unreachable`, plus
`not_found`, `ambiguous_run_id`, `ipc_deadline`, `incompatible_contract`, and
`peer_identity_unsupported`) instead of a sentence: a **stale registry
entry** (the runner died abruptly, so the entry's record is left behind but
its liveness lock has been released, detected *before* connecting), an
**unprobeable registry entry** (the liveness lock could not be probed at all,
so the runner is *not* confirmed gone — the message says liveness could not be
probed and calls the entry `unprobed`, never "the runner is gone"), or **died
mid-conversation** (the entry read live, but the runner exited between the
liveness probe and the reply, or the connection closed before a complete
response arrived). All three are bounded — no client hangs waiting for a
runner that is not going to answer. `list` is the fastest cross-check for the
first two without retrying the failing command, and it reports the same
verdict the refusal did: a stale entry shows as `stale`, an unprobeable one as
`unprobed` (see "`list` shows an entry as `unprobed`" above for what to do
with that one — in short, do not hand-delete it). See
[`docs/control-plane.md`](control-plane.md), "When the runner cannot be
reached: a distinguishable result, never a hang", and the `CONTROL` (103) row
of the reserved-band table in [`docs/exit-codes.md`](exit-codes.md).

**The read-only verbs add a refusal the three above cannot produce, and it is not
a lost runner.** If the message says the runner answered with a **control-plane
version** this client does not read (`kind: "incompatible_contract"`), the runner
is reachable and healthy: the exchange completed, and it is the *answer* that was
rejected, because it declares a contract this binary does not implement — in
practice a runner newer than the client you are running. `inspect` and `attest`
each carry a version axis of their own, and either one can be refused this way —
the same `kind`, a different contract, a different fix:

- `inspect` says "the runner answered with control-plane **snapshot** version …",
  naming the number that arrived and the range this build reads. Retrying will not
  help: inspect that run with a build that implements its snapshot version — for a
  newer runner, one at least as new as the binary that started the run.
- `attest` says "the runner answered with control-plane **attestation** version …",
  naming the number that arrived and the single version this build reads. There is
  no read-down range on that axis on purpose — a misread membership verdict is a
  security answer rather than a diagnostic — so retrying is equally useless: attest
  that run with a build that implements its attestation version.

`cancel`/`kill` cannot hit either refusal (their ack carries no version), and
neither can `list`/`wait`/`prune`, which ask no runner for one, so in both cases the
run itself is still fully controllable. `processkit-cli probe --json` reports the
`version` of **whichever binary you run**, which is how you tell two installed
builds apart (no preflight can report a *runner's* snapshot or attestation version —
those numbers only arrive in its reply). See
[`docs/control-plane.md`](control-plane.md), "Snapshot version: a newer runner's
reply is refused, an older one is read" and "Attestation version", and
[`docs/integration.md`](integration.md) §6 ("An unreadable contract version").

**Beyond that shared refusal, `attest` has one more of its own, and it is neither
a lost runner nor a negative verdict.** If the message says the runner could not
obtain a kernel-authenticated identity for this client from the control transport
(`kind: "peer_identity_unsupported"`), the runner is reachable and healthy: the
exchange completed, and it *declined to decide* membership because a transport
that cannot name the caller leaves it nothing to decide on — reporting an
unproven `member` is the one answer it will not give. This is a refusal, never a
"no": a **decided** non-membership is not a `103` at all but `NOT_A_MEMBER`
(115), `kind: "not_a_member"` (the attestation is printed on stdout either way).
Retrying will not help, because the capability is a property of the runner's
platform and build rather than of the moment; rule it out at preflight instead,
with `processkit-cli probe --json --require-surface attest:peer-identity` run
against the runner's own binary — meeting it at runtime means that check was
skipped, or the runner is a different build. No other command can hit it:
`inspect`/`cancel`/`kill`/`wait`/`list` never ask the question, and the run
itself is untouched and still going, since `attest` is read-only. See
[`docs/control-plane.md`](control-plane.md), "`attest`", and
[`docs/integration.md`](integration.md) §6 ("A caller the runner cannot name").

**`wait` does not share this code.** The registry-only `wait --run-id <id>`
never connects to a run's control transport, so "died mid-conversation" is
not something it can hit, and a stale registry entry does not give it `103`
either — only the *ambiguous-`run_id`* reason below does. A stale or missing
entry makes `wait` exit `0`, the same as a run that finished cleanly (the
registry keeps no history, so "`build-42` was never registered" and
"`build-42` finished a moment before you asked" read identically): do not
take a `0` from `wait` as proof a stale-looking `run_id` was ever live. See
[`docs/registry.md`](registry.md#an-unknown-run_id-reads-as-finished).

**Not a run outcome.** A `103` says nothing about how the target run itself
ended (or whether it is still running) — for the by-`run_id` form it is purely
"this client could not resolve or reach a single target run". Do not conflate
it with the run-outcome codes (`106`–`109`, or the child's own code), which
come only from the run's own process exit. (`cancel --all` / `kill --all`'s
`103` is different — see below.)

## `cancel --all` / `kill --all` and a partial `103`

**Symptom.** `cancel --all` or `kill --all` exits `103`, but the JSON
report it printed to stdout shows at least one target with `"accepted": true`
— so, unlike the by-`run_id` form's `103` above, this run **was** acted on.

**Diagnose.** `--all`'s `103` is a different fact from the by-`run_id` form's:
it means "one or more targets in the confirmed-live snapshot could not be
reached or did not acknowledge the command", not "nothing was found or
reached". A snapshot with several live runs commonly has a mix — most targets
ack cleanly while one becomes unprobeable, changes identity, or does not respond in
time — and the aggregate exit code reflects that *any* failure occurred so an
automated caller never mistakes a partial teardown for a complete one. Read
the JSON report on stdout (not just the stderr tally) to see exactly which
records failed and why; stderr on its own only gives the failure count. A duplicate
`run_id` is not itself an aggregate failure because `--all` addresses each record
path and endpoint independently. Likewise `status: "already_gone"` is non-error: the
target ended after the snapshot, so `accepted` is false but teardown is already
complete for it. See
[`docs/control-plane.md`](control-plane.md), "`cancel --all` / `kill --all`",
and the `CONTROL` (103) row of the reserved-band table in
[`docs/exit-codes.md`](exit-codes.md).

**Fix.** Re-running `cancel --all` / `kill --all` is safe: a target already
ended is absent from the next snapshot (or `already_gone` if it ended during this
one), and a target that failed for a
transient reason (e.g. it was mid-exit) is retried. If a specific `run_id`
keeps failing, use `list --json` or `inspect --run-id <id>` to see why (a
stale entry, an unprobed one, or a genuine ambiguity — the same reasons the
by-`run_id` sections above and below cover).

## An ambiguous `run_id`

**Symptom.** `inspect` / `cancel` / `kill` / `attest` / `wait --run-id <id>`
exits `CONTROL` (`103`) with an "ambiguous run id" message, even though you
believe exactly one run with that id is alive.

**Diagnose.** The registry does **not** enforce `run_id` uniqueness at
`register` time: two runs started concurrently with the same explicit
`--run-id` are both written as independent, live entries. Every by-`run-id`
client — including the read-only `inspect` and `attest`, and the registry-only
`wait` — fails closed with `CONTROL` (`103`) the moment more than one **live** entry
matches, rather than silently acting on whichever entry a directory scan
happens to return first; run `list --json` and filter by `run_id` to see the
duplicates directly. See [`docs/registry.md`](registry.md), "Run id
resolution — ambiguity is a hard failure".

**Fix.** Keep `run_id`s unique among your own concurrently-live runs (a
counter, a UUID, or any value your launcher does not reuse before the
matching run has ended); there is no way to disambiguate after the fact
other than avoiding the collision at launch time.

## The child's terminal behavior degrades under the default pipe + echo

**Symptom.** Colors, progress bars, spinners, or other cursor-based rendering
from the child look wrong, missing, or replaced with plain line-by-line
output — even though the same command renders correctly when run directly in
a terminal.

**Diagnose.** By default `run` gives the child **pipe + echo, not a real
inherited terminal**: ProcessKit reads the child's stdout/stderr through
pipes and this runner re-emits the bytes onto its own stdout/stderr. The
child therefore sees no TTY on either stream, so any code path in it that
checks `isatty()` (or equivalent) before drawing takes its non-interactive
branch — this is the child's own, otherwise-correct terminal detection
working as designed, not a bug in the runner's pump.

**Fix.** Pass `--inherit-stdio` for an interactive command: it hands the
child the runner's own stdin, stdout, and stderr handles directly — no pump,
no echo, no `--capture-dir` tee in this mode — so an existing terminal is
preserved unmediated instead of proxied. It is mutually exclusive with
`--capture-dir`, `--create-no-window`, `--inherit-stdin`, `--stdin-file`,
`--no-echo`, `--idle-timeout`, and `--detach` (a detached run has no terminal
to hand over in the first place); `Ctrl-C` behavior also becomes
platform-dependent under this flag rather than the runner's own uniform
`cancelled`/`107` outcome. See `README.md`, "Standard I/O", for the full
contract, including exactly how `Ctrl-C` is delivered in this mode on each
platform.

## See also

- [`docs/schema.md`](schema.md) — the normative JSONL event schema.
- [`docs/exit-codes.md`](exit-codes.md) — the normative reserved exit-code
  band.
- [`docs/registry.md`](registry.md) — the normative registry location,
  staleness signal, and reaping rules.
- [`docs/control-plane.md`](control-plane.md) — the normative local
  transport, wire protocol, and `inspect`/`cancel`/`kill`/`attest` behavior.
- [`docs/integration.md`](integration.md) — the consumer/adapter walkthrough,
  organized by call sequence rather than by symptom.
- [`docs/platform-support.md`](platform-support.md) — what each platform
  guarantees, and which of those guarantees `doctor` confirms on the machine in
  front of you.
