# 0006: Poll the registry for detached waits

- Status: Accepted
- Date: 2026-07-29 (retrospective)

## Context

A detached caller is not the runner's parent and cannot wait on its process handle.
The control channel is deliberately tied to a live runner and may be busy, while the
registry already owns the authoritative live/stale/unprobed classification needed
for discovery.

## Decision

Implement `wait` as bounded polling of the owner-only registry, without contacting
the runner or mutating registry state. For `--run-id`, require one unambiguous live
record. For `--all`, snapshot the exact confirmed-live record paths once and poll
only that finite set; registrations after the snapshot are outside the wait.

The full state model and timeout distinction are documented under
[Waiting](../registry.md#waiting--wait) and in the [exit-code contract](../exit-codes.md#a-waiters-deadline-is-not-a-runs-deadline).
The polling and snapshot rules are implemented in
[wait.rs](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/src/wait.rs).

## Alternatives considered

- Hold a control-plane connection until exit. Rejected because wait needs no live
  group operation and would couple observation to the sequential IPC server.
- Wait on runner PIDs. Rejected because the caller owns no process handle and PID
  reuse would weaken identity.
- Re-scan all matching run ids on every aggregate poll. Rejected because new runs
  could extend `wait --all` forever and duplicate ids would create false ambiguity.

## Consequences

Wait is read-only, restartable, and independent of control-channel availability.
Polling introduces bounded detection latency. Its own deadline returns
`WAIT_TIMEOUT` without ending any run, and unprobed registry state must remain
honest rather than being treated as confirmed completion.
