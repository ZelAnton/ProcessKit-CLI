# Resource limits

Three `run` flags request kernel-enforced limits over the whole contained tree:

| Flag | Scope | Grammar |
| --- | --- | --- |
| `--max-memory SIZE` | Total tree memory | Bytes, or `k` / `m` / `g` binary units |
| `--max-processes N` | Live processes in the tree | Positive integer |
| `--cpu-quota CORES` | CPU relative to one core | Finite number greater than zero |

```sh
processkit-cli run \
  --max-memory 1g \
  --max-processes 32 \
  --cpu-quota 1.5 \
  --jsonl limited.jsonl \
  -- compiler-worker
```

Omitting a flag leaves that resource unbounded. The runner never invents a
default cap.

## Fail closed, before spawn

A requested limit is a requirement, not a hint. If the active platform and
containment mechanism cannot enforce it, `run`:

1. does not spawn the child;
2. emits `limit_hit` naming `memory`, `processes`, or `cpu`;
3. emits `container_failed` and terminal `runner_exit`;
4. exits `BACKEND` (`102`).

An adapter must inspect `limit_hit`; code `102` also covers unrelated backend
failures.

## Platform matrix

| Mechanism | Memory | Process count | CPU | Notes |
| --- | --- | --- | --- | --- |
| Windows Job Object | Yes | Yes | Yes | Whole-job enforcement. |
| Linux cgroup v2 with usable controllers | Yes | Yes | Yes | Requires controller delegation at the effective root. |
| Linux process-group fallback | No | No | No | Fails before spawn. |
| macOS / BSD process group | No | No | No | Fails before spawn. |

The `run_started.mechanism` field tells an observer what was actually obtained.

## Linux controller requirements

The current ProcessKit implementation can apply limits when the runner is a
direct member of the real cgroup-v2 root and can enable the required
controllers. This is common in a minimal init environment, but not in:

- a normal systemd user session, scope, or service;
- ordinary Docker or Kubernetes containers;
- typical GitHub Actions jobs;
- any environment that delegates a nested cgroup without writable controllers.

In those environments a limit request fails rather than falling back to an
unenforced process-group run.

## Linux process-count caveat

The cgroup `pids` controller reliably bounds descendants forked inside the
cgroup. It does not reject additional top-level launches into the same group in
the same way Windows Job Object active-process limits do.

For ProcessKit CLI, which launches one root per run, interpret
`--max-processes` as a cap on that tree's own growth. It protects against a
contained fork explosion; it is not a general admission controller for unrelated
launchers.

## Size parsing

Units are binary:

| Input | Bytes |
| --- | ---: |
| `1048576` | 1,048,576 |
| `512k` | 524,288 |
| `256m` | 268,435,456 |
| `2g` | 2,147,483,648 |

Zero, malformed values, and overflow are usage failures (`100`). CPU quota also
rejects negatives, `NaN`, and infinities.

## Applied limit versus observed limit hit

`limit_hit` proves only that a requested limit **could not be applied before
launch**. It does not describe a successfully installed cap firing later, and
its payload and meaning remain unchanged for compatibility.

With `processkit` 3.2.0, a run that requested at least one cap emits a separate
`limit_evidence` event after the child ending is known and immediately before
the teardown pair. It carries one verdict for each axis (`memory`, `processes`,
and `cpu`):

This event exists only when `ProcessGroup::with_options` successfully creates a
container. On macOS, the BSDs, and the Linux process-group fallback, ProcessKit
returns `ResourceLimit` during group creation because that mechanism has no
whole-tree limit primitive. The runner therefore emits the existing pre-spawn
`limit_hit` event and its backend-error tail, but there is no group from which to
read `limit_evidence`; the event is not emitted on that path. `unknown` is
reserved for a successfully created group whose active mechanism cannot provide
the post-run answer.

- **Three-valued, never a boolean.** The JSONL `limit_evidence` event represents
  `Tripped` / `NotTripped` / `Unknown` as `tripped` / `not_tripped` / `unknown`.
  `Unknown` never collapses into "did not fire": that would silently misreport
  a platform's inability to answer as a clean run on every axis where evidence
  is unavailable.
- **Authoritative on Linux cgroup v2 only.** There, `Tripped`/`NotTripped`
  come from real kernel counters (`memory.events`' `oom`, `pids.events`'
  `max`, `cpu.stat`'s `nr_throttled`). On Windows Job Object and on a POSIX
  process group that was successfully created, a capped axis instead reports
  `Unknown` as a *measured* result, not an omission — those mechanisms keep no
  post-mortem record that a cap fired. In practice the POSIX limit request fails
  during group creation as described above, so POSIX fallback runs have no
  post-run event at all. Windows is a first-class platform for this CLI, and
  runtime limit attribution remains `unknown` there; this closes the gap on
  Linux cgroup v2 only.
- **Readable only while the container still exists.** The evidence lives in
  the container itself, so the runner reads it before `ProcessGroup` is dropped
  or consumed by shutdown. `limit_evidence` therefore precedes
  `cleanup_started`, preserving the `cleanup_started` → `cleanup_finished`
  ordering.

The event is absent when no cap was requested. On an event that is present,
uncapped axes are reported as `not_tripped` by ProcessKit because nothing was
in force that could fire; `unknown` is reserved for a missing authoritative
answer from the active mechanism.

None of this changes what `limit_hit` means today: it stays the pre-spawn
"the requested cap could not be applied" event, and a cap-dependent adapter
still treats it as a hard failure signal in that scope (see
[`docs/schema.md`](schema.md#limit_hit)).

## What the tree consumed

`limit_hit` and `limit_evidence` are both about a **cap**: one says it could not be
installed, the other whether it fired. Neither says how much the tree actually used.
That is the separate `resource_summary` event, which every run that spawned a child
emits exactly once — no flag, no cap required, every platform (normative field list:
[`docs/schema.md`](schema.md#resource_summary)). With `processkit` 3.3.0 it takes one
`ProcessGroup::stats()` reading of whatever the active mechanism accounts for — a Job
Object's own accounting block on Windows; on Linux the cgroup's `io.stat`/`pids.peak`
counters plus a per-member `/proc` sum for memory and CPU, which is why the matrix below
ties those two axes to the read point — at the same place in the teardown tail as
`limit_evidence` and immediately after it.

### What it does not prove

- **It is not limit attribution.** A `peak_memory_bytes` at or near a requested
  `--max-memory` is not evidence the cap engaged; only `limit_evidence`'s `tripped` is,
  and only where that verdict is authoritative (Linux cgroup v2). Reading a high peak
  as "it was capped" would invent an attribution the kernel never made.
- **It is not a time series.** One reading at the end of the run, not a sample. It
  cannot say *when* the peak occurred, or what the tree looked like at any earlier
  moment. `run --snapshot-interval` answers the second of those — the tree's *shape* over
  time, and it is opt-in precisely because that has an ongoing cost — but it carries no
  resource numbers, so nothing in this stream is a consumption series.
- **It is not a per-process breakdown.** Every number is whole-tree. A member's
  individual share is not recoverable from it.
- **It does not measure disk.** Bytes written to a capture file are
  `output_captured.bytes` (see [`docs/io-and-capture.md`](io-and-capture.md)); the IO
  counters here are the tree's own read/write traffic and are a different quantity that
  happens to share a unit.

### Platform matrix

Every measurement is independently nullable, and `null` means **this mechanism, at this
read point, does not account for it** — never zero, and never a number the runner
improved by taking a maximum over its own periodic reads (that would report when the
runner looked, not what the tree did). Two of the axes below depend on the read point and
not on the platform alone, so the "mechanism" column is a *necessary* condition for a
number, never a sufficient one. This matrix is normative; do not read completeness into
the event's field list.

| Mechanism | `peak_memory_bytes` | `total_cpu_ms` | `io_read_bytes` / `io_write_bytes` | `peak_process_count` |
| --- | --- | --- | --- | --- |
| Windows Job Object | Yes — peak *committed* memory (`PeakJobMemoryUsed`) | Yes — every process ever in the job, terminated ones included | Yes — `IO_COUNTERS`, **all** read/write traffic (file, pipe, device) | **Always `null`** |
| Linux cgroup v2 | Only for members **live at the read point** — the sum of their `VmHWM`; `null` once the tree has exited, which is the natural-exit case (consequence 5) | Only for members **live at the read point**; `null` once the tree has exited (consequence 5) | Only with the `io` controller enabled — `io.stat` `rbytes`/`wbytes`, **block layer only** | Only with the `pids` controller enabled — `pids.peak` |
| Linux process-group fallback | `null` | `null` | `null` | `null` |
| macOS / BSD process group | `null` | `null` | `null` | `null` |

Five consequences that are easy to misread as bugs:

1. **`peak_process_count` is always `null` on Windows.** A Job Object keeps
   `ActiveProcesses` (how many are in it now) and `TotalProcesses` (how many were ever
   assigned to it). Neither is a high-water mark of concurrency, and this runner will
   not synthesize one from its own `stats()` calls.
2. **IO bytes are always `null` on macOS, the BSDs, and the Linux process-group
   fallback.** Those mechanisms contain a tree without accounting for it. On Linux
   cgroup v2 they additionally require the **`io` controller** to be enabled for the
   group's cgroup — which is what makes `io.stat` exist at all. This CLI does not
   enable it: `processkit` enables exactly the controllers a requested `ResourceLimits`
   needs (`memory`, `pids`, `cpu`) and no others, so `io` is on only if the environment
   already delegated and enabled it. The same is true of `pids.peak`, which needs the
   `pids` controller — in practice that means a run with `--max-processes`.
3. **The IO counters are not comparable *across* platforms.** A Job Object counts bytes
   moved by every read/write the job's processes issued, whatever the target. A cgroup's
   `io.stat` counts only what crossed the **block layer**, so a read served from the
   page cache, or any traffic over a pipe, socket, or tmpfs, is simply not in it. The
   same workload legitimately reports very different numbers on the two, and neither is
   wrong. Compare a series only against itself on one mechanism; read
   `run_started.mechanism` to know which one produced it.
4. **On Linux, `io_write_bytes` can undercount.** A write reaches the block layer when
   the kernel writes the page back, which may be after the member that dirtied it
   exited — or never, if the page is still dirty when the group is torn down. A short
   write-and-exit run can report fewer bytes than it handed to `write(2)`.
5. **On Linux cgroup v2, `peak_memory_bytes` and `total_cpu_ms` are `null` after a
   natural exit** — the most common ending, so this is the ordinary reading there and
   not a corner case. Unlike the two axes beside them, these are not counters the
   cgroup keeps: ProcessKit sums them out of `/proc/<pid>` over the members listed in
   `cgroup.procs` **at the moment of the read**, and it does so whether or not a cap
   was requested (`memory.peak` and `cpu.stat` are never consulted, so `--max-memory`
   does not change it). A process leaves `cgroup.procs` as soon as it exits — a zombie
   never appears there — and this read happens *after* the ending is decided, so on
   the natural-exit path the child has already exited and been reaped and there is
   normally nothing left to sum. Both axes then come back `null` with `read_error:
   false`, which is a correct answer about that read point and not a failure. Two
   corollaries follow, and the second is the useful one:
   - A child that leaked a descendant which outlived it makes the two axes
     **non-`null`** — but they then cover only the survivor, arbitrarily far below what
     the tree as a whole used. A small number here is not a whole-tree total.
   - On a runner-imposed ending (`timeout`, `cancelled`, `killed`, `output_overflow`)
     the read happens *before* the soft stop, while the tree is still running, so there
     both axes are populated. If a workload's memory or CPU is what you need on Linux, a
     run the runner itself ended is the only place this stream carries it: no other event
     does, `members_snapshot` (including `--snapshot-interval`'s samples) carrying member
     identity only — `pid`, `ppid`, `name`, `start_time`.

   Neither Windows nor Linux's other two axes are affected. A Job Object's accounting
   block outlives the processes charged to it, so its memory and CPU cover the whole
   job whatever the ending; `io_read_bytes`/`io_write_bytes` and `peak_process_count`
   on Linux *are* cgroup-kept counters and likewise survive their members — whether
   they are present is the controller question in consequence 2.

`peak_memory_bytes` and `total_cpu_ms` are likewise platform-specific in *meaning*
(committed memory vs. resident high-water mark; the whole job's history vs. only the
members live at the read point), so the same caution applies to them: comparable
within a mechanism, not across.

### A failed read is in the stream, not missing from it

If `stats()` fails, the event is still emitted with `read_error: true` and every
measurement `null`. Check that flag before drawing a conclusion from a `null`, because
an all-`null` summary is *also* a correct success — it is exactly what row 3 and row 4
of the matrix above report by design, **and what row 2 reports as well** for the
commonest case of all: a plain `run` on Linux cgroup v2 that ended by its child exiting
has no live member left to sum for memory and CPU (consequence 5) and — unless the
environment itself enabled the `io` controller, and `--max-processes` the `pids` one —
no container counter to answer for the other three (consequence 2), so all five
measurements are `null` with `read_error: false`. An all-`null` summary therefore
carries no information about whether the read worked, on any platform: `read_error` is
the only thing that separates "this mechanism, at this read point, accounts for
nothing" from "the read failed", and a foreground run's stderr warning does not help a
`--detach` run, whose stderr is null.

### Preflighting it

`resource_summary` is present on every build that has it, so a consumer pins the
**event**, not a platform:

```sh
processkit-cli probe --json --require-surface run:resource-summary
```

That token's presence guarantees the event will be in the stream. It does **not**
promise any particular axis is populated — that is what the matrix above governs, and it
follows from `run_started.mechanism`, plus (on Linux cgroup v2, for memory and CPU) from
how the run ended. Never from a `probe` token.

## Limits and outer containers

An outer Docker/Kubernetes/systemd limit and a ProcessKit CLI limit are separate
layers. The stricter layer wins, but only the outer runtime can explain its own
termination reason. If the outer runtime kills the runner itself, the
platform-specific `abrupt_cleanup` contract applies.

Use outer-runtime limits when they are the authoritative scheduler policy. Use
CLI limits only where ProcessKit can install them and the adapter needs the
limit request attached to this specific run.

## Operational checklist

1. Run `probe --json` to verify the flags exist.
2. Launch a harmless limited command in the real deployment environment.
3. Read `run_started.mechanism` rather than assuming cgroup availability.
4. Treat pre-spawn `limit_hit` as a hard configuration failure.
5. For a successfully created capped run, read `limit_evidence` for post-run
   attribution and preserve `unknown` as distinct from `not_tripped`. For a
   pre-spawn `limit_hit`, do not expect post-run evidence: the container did not
   exist to be queried.
6. For actual consumption, read `resource_summary` — present on every run that
   spawned a child, capped or not. Check `read_error` first, then treat each `null`
   as "this mechanism does not account for it *at this read point*" per the matrix in
   "What the tree consumed", never as zero, and never compare its IO counters across
   platforms. On Linux cgroup v2 in particular, do not expect `peak_memory_bytes` or
   `total_cpu_ms` from a run whose child exited on its own — there they are `null` by
   construction (consequence 5).
7. Keep a separate outer-runtime signal for limits imposed outside this run.

## See also

- [Platform support](platform-support.md) — mechanism selection.
- [Running in containers](containers.md) — cgroup delegation in images and
  orchestrators.
- [JSONL event schema](schema.md#limit_hit) — normative event fields; see also
  [`resource_summary`](schema.md#resource_summary) for what the tree consumed.
- [Exit-code contract](exit-codes.md#resource-limits-reuse-backend-102).
