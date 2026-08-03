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

- **Three-valued, never a boolean.** The JSONL `limit_evidence` event represents
  `Tripped` / `NotTripped` / `Unknown` as `tripped` / `not_tripped` / `unknown`.
  `Unknown` never collapses into "did not fire": that would silently misreport
  a platform's inability to answer as a clean run on every axis where evidence
  is unavailable.
- **Authoritative on Linux cgroup v2 only.** There, `Tripped`/`NotTripped`
  come from real kernel counters (`memory.events`' `oom`, `pids.events`'
  `max`, `cpu.stat`'s `nr_throttled`). On Windows Job Object and on a POSIX
  process group (macOS, the BSDs, the Linux process-group fallback), every
  capped axis instead reports `Unknown` as a *measured* result, not an
  omission — those mechanisms keep no post-mortem record that a cap fired.
  Windows is a first-class platform for this CLI, and runtime limit attribution
  remains `unknown` there; this closes the gap on Linux cgroup v2 only.
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
5. Read `limit_evidence` for post-run attribution, and preserve `unknown` as
   distinct from `not_tripped`.
6. Keep a separate outer-runtime signal for limits imposed outside this run.

## See also

- [Platform support](platform-support.md) — mechanism selection.
- [Running in containers](containers.md) — cgroup delegation in images and
  orchestrators.
- [JSONL event schema](schema.md#limit_hit) — normative event fields.
- [Exit-code contract](exit-codes.md#resource-limits-reuse-backend-102).
