# 0003: Keep control in the live runner

- Status: Accepted
- Date: 2026-07-29 (retrospective)

## Context

`inspect`, `cancel`, and `kill` need access to the exact ProcessKit group owned by a
specific live run. A PID can be reused, and named kernel objects would expose a
second lifecycle and security surface with platform-specific ownership semantics.

## Decision

Host the control server inside the live `run` process. Resolve a `run_id` through an
owner-only per-user registry, then connect to the recorded Unix-domain socket or
Windows named pipe. Reconfirm the exact registry record around dispatch. Treat a
dead runner as stale; never reconstruct control from a PID.

Protocol and snapshot details live in the [control-plane guide](../control-plane.md),
with lifecycle discovery in the [registry guide](../registry.md).
The stable implementation entry points are the
[control facade](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/src/control/mod.rs)
and [registry facade](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/src/registry/mod.rs).

## Alternatives considered

- Address the root PID directly. Rejected because PID reuse can redirect a command
  to an unrelated process and cannot recover the owned container handle.
- Publish globally named Job Objects or other kernel containers. Rejected because
  semantics and ACLs diverge across platforms and outlive the runner differently.
- Run a permanent daemon. Rejected because the one-command runner does not require
  a new service lifecycle or privileged broker.

## Consequences

Only the process that owns the container can answer or mutate it. IPC and registry
artifacts need owner-only permissions and bounded conversations. Abrupt runner death
makes control unavailable but leaves a detectable stale record and the platform's
documented abrupt-cleanup guarantee.
