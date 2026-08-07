# 0008: Do not expose external PID adoption in the CLI

- Status: Accepted
- Date: 2026-08-07

## Context

ProcessKit 3.3.0 adds the public
[`ProcessGroup::adopt_external`](https://docs.rs/processkit/3.3.0/processkit/struct.ProcessGroup.html#method.adopt_external)
method. It accepts only a `pid` and lets a caller place an already-running
external process into a `ProcessGroup`. This is useful to a supervisor that has
the process number but does not hold a `tokio::process::Child`.

The CLI's existing ownership model is different. A `run` invocation starts its
child, keeps the live runner as the owner of the ProcessKit container, and
forwards the child's exit code. Other commands discover that run by `run_id`
through the owner-only registry and reach the endpoint published by that live
runner. [ADR 0003](0003-live-runner-control-plane.md) therefore says that
control is hosted in the live runner and is never reconstructed from a PID;
[ADR 0004](0004-container-scoped-cleanup.md) limits cleanup to the current
run's owned container.

`adopt_external` is deliberately a lower-level capability, not an ordinary
child-launch operation:

- A PID is used as a one-time lookup, not retained as the identity. ProcessKit
  captures its own identity anchor: a process object held after `OpenProcess`
  on Windows; a start-time token around the migration on Linux cgroup v2; and a
  start-time token for tracked entries on POSIX process-group backends. Later
  probes, signals, and teardown are bound to that anchor, so PID reuse after
  the adoption call does not redirect those operations.
- That anchor cannot close the caller's pre-adoption TOCTOU window. A PID read
  from a pidfile, registry, or another supervisor may already identify a new
  process by the time the caller passes it to `adopt_external`. A token read by
  the caller earlier is not an improvement: it is older than the anchor taken
  by ProcessKit and would require every caller to duplicate platform-specific
  identity rules. ProcessKit's own documentation calls this residual window
  out explicitly.
- The API never receives a `Child`, never waits for the adopted process, and
  never exposes its exit status. It can list and signal membership, including
  hard-kill and drop cleanup, but it cannot produce the child's exit code. An
  adopted process that exits during the adoption call can also produce `Ok`
  with nothing left to contain. These are not the semantics of the CLI's
  ordinary `run` path, whose exit-code and `runner_exit.child_code` contract is
  defined around an actual child.
- Adoption is not neutral toward an existing supervisor. Only the target
  process is moved; descendants it already spawned keep their old containment,
  while future forks follow the new mechanism. The target's existing
  containment can also be changed by the platform.

The question is therefore not whether the ProcessKit method is technically
available. It is whether this CLI should turn a destructive, PID-selected
ownership transfer with no exit-status channel into a public command contract.

## Decision

`processkit-cli` will **not expose external PID adoption** in its current public
CLI. In particular, this repository will not add:

- an `adopt --pid <pid>` subcommand;
- a `run --pid <pid>` or `run --adopt-pid <pid>` mode; or
- PID-targeted variants of `inspect`, `cancel`, or `kill`, or registry records
  keyed by `root_pid`.

The existing boundary remains normative: a CLI control operation resolves a
`run_id`, then uses the endpoint in that run's registry record to reach the live
runner, and the live runner acts only on its own ProcessKit container. The
registry remains PID-free for addressing, and the JSONL lifecycle schema and
exit-code fidelity contract remain unchanged.

This is a refusal of a CLI feature, not a rejection of the ProcessKit API. A
caller that explicitly owns the cross-supervisor agreement may use
`adopt_external` directly through ProcessKit. That caller must also own the
missing wait/exit-status semantics and the consequences of changing the target's
existing containment. Reconsidering a CLI feature requires a separate ADR and a
new implementation task; it must not be smuggled into the existing `run` or
control-plane contracts.

## Alternatives considered

### Separate `adopt --pid` subcommand

This is the clearest of the positive CLI shapes, because it could make the
ownership transfer and the lack of a child exit code explicit. It still accepts
a destructive PID from outside the CLI's registry, cannot eliminate the
pre-adoption TOCTOU window, and would need a second lifecycle contract for a
process whose parent and exit status belong elsewhere. It would also need to
answer whether the resulting group is a normal `run` for `list`, `inspect`,
`cancel`, `kill`, `wait`, `members_snapshot`, and `runner_exit`; answering those
questions consistently would create a second run kind rather than a small
adapter around ProcessKit. Rejected.

### A flag on `run`

`run --pid <pid>` or `run --adopt-pid <pid>` would reuse the existing command
name, registry entry, control endpoint, JSONL stream, and teardown code. That
reuse is misleading: there is no command to spawn, no `Child` to wait for, no
child exit code to forward, and no truthful ordinary `runner_exit` with a
`child_code`. It would also make the meaning of stdin/stdout capture, timeout,
`root_pid`, and the initial `members_snapshot` depend on whether a hidden
alternative input was selected. Rejected.

### PID arguments on the control plane or in the registry

Adding `--pid` to `inspect`, `cancel`, or `kill`, or publishing `root_pid` as a
target key, would bypass the live-runner endpoint and turn a run-control
interface into a process-table interface. It would directly contradict the
registry's [No PID addressing](../registry.md#no-pid-addressing) rule and make a
PID reuse mistake an action against an unrelated process. Rejected even if
adoption itself were otherwise available.

### Expose the capability now and document the caveats

Documentation would not make the pre-adoption window, missing exit status, or
foreign-container side effects disappear. A public CLI would invite scripts to
treat a best-effort process lookup as an ownership boundary that the CLI cannot
prove. Refusal is the only option that preserves the already-published
control-plane and cleanup contracts without inventing a second, weaker one.
Chosen.

## Platform-specific risks

The API's identity anchor makes later operations PID-reuse-safe within its
limits; it does not make cross-supervisor adoption portable or ownership-neutral:

| Platform/mechanism | ProcessKit 3.3 behavior | CLI consequence |
| --- | --- | --- |
| Windows `JobObject` | `OpenProcess` obtains a handle with the rights needed for assignment and termination, then the handle's process object is assigned to the job. A target already in another job may cause this job to nest under the outer job. On Windows 11, adoption of an outer-job member succeeded while the new group was empty, while the same operation after this group had spawned its own member was observed to fail with `ERROR_ACCESS_DENIED`. | A successful adoption can make an unrelated outer supervisor's termination and limits reach this CLI's future members. A refusal can depend on call order and foreign job state, not merely on the PID. |
| Linux cgroup v2 | ProcessKit reads `/proc/<pid>/stat`, writes the PID to this group's `cgroup.procs`, then reads the start-time again. cgroup membership is exclusive: the target leaves its previous cgroup, so that supervisor's limits and teardown no longer apply. Existing descendants are not moved. If a recycle is detected after the write, ProcessKit attempts a best-effort move-out; if that fails, the target remains in this group's teardown scope. | Adoption can change another supervisor's containment and can leave the target owned by this group's teardown after a failed rollback. The post-call identity check detects a race; it cannot undo every kernel-level side effect or restore the unknown cgroup the target originally occupied. |
| Linux process-group fallback and macOS | ProcessKit captures a start-time identity and normally tracks an external target individually because `setpgid` is not permitted for a process this caller did not start. Later signals are identity-gated, but descendants already present remain outside and future forks are not included. The token's platform resolution is finite, so same-tick occupants remain an upstream caveat. | The CLI could kill the adopted process without containing its already-existing tree, while a normal `run` promises container-scoped tree cleanup. On the process-group fallback, an unreaped external process can remain a zombie through the grace period because this API cannot reap it. |
| FreeBSD and other BSDs | No start-time reader is wired into the public adoption path, so `adopt_external` returns `ErrorReason::Unsupported` rather than tracking a bare number. FreeBSD's process reaper covers descendants of this process, not an external supervisor's process. | A supposedly portable CLI feature would either fail on these targets or tempt a caller-side PID-only fallback, which the project must not provide. |

These differences are material even before considering permissions: Windows may
deny opening a process owned by another user, integrity level, or protection
class; Linux may deny the cgroup write or identity read; and a POSIX identity
read may be unavailable under a restricted `/proc` or `proc_pidinfo` policy.

## Consequences

The CLI keeps one ownership model and one addressing model. `run` continues to
mean "start and supervise this child"; `inspect`, `cancel`, `kill`, and `wait`
continue to resolve a `run_id`; and cleanup continues to be delegated to the
ProcessKit container owned by that run. No new CLI flag, registry field, JSONL
event, snapshot version, exit-code kind, or compatibility surface is required.

The cost is deliberate: a process started by another supervisor cannot be
handed to `processkit-cli` for tree cleanup. Integrators that need that workflow
must either make the CLI the process's original supervisor or use ProcessKit's
lower-level API with an explicit agreement about containment transfer, waiting,
and exit-status ownership. The CLI will not infer that agreement from a PID.

No roadmap delivery item is changed by this decision: the roadmap has no
scheduled external-adoption feature, so no `docs/ROADMAP.md` edit is needed.

If this decision is revisited, the follow-up must be a distinct feature design
with cross-platform tests for PID reuse and pre-call races, foreign Job Object
and cgroup membership, missing identity readers, descendants that predate
adoption, zombie behavior, and an outcome contract that never pretends an
adopted process has a reapable child exit code. It must also specify how a
future JSONL/control-plane contract represents "membership is observable but
exit status is not" before any production code is written.
