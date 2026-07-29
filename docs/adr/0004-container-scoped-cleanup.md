# 0004: Scope cleanup to owned containers

- Status: Accepted
- Date: 2026-07-29 (retrospective)

## Context

Build tools commonly reuse generic executable names, and unrelated users or runs
may host identical workers. Name-based cleanup and recursive PID walking are both
racy and can terminate processes outside the invocation that requested containment.

## Decision

End only members of the current run's ProcessKit container. Let ProcessKit remain
the source of truth for membership, graceful stop, escalation, PID-reuse discipline,
and kill-on-drop. Never clean up by executable name or by an externally supplied PID.

The ownership boundary is summarized in
[Architecture](../architecture.md#boundary-with-processkit)
and operational teardown in [Timeouts and cancellation](../timeouts-and-cancellation.md).
The CLI delegates the lifecycle to the
[run path](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/src/run/mod.rs) and its
[ProcessKit-backed teardown](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/src/run/teardown.rs).

## Alternatives considered

- Kill every process with a known worker name. Rejected because it cannot
  distinguish this run from unrelated builds.
- Walk descendants from the root PID in the CLI. Rejected because membership races
  process exit/reuse and duplicates core ProcessKit semantics.
- Reimplement missing containment behavior locally. Rejected because two sources of
  teardown truth would inevitably disagree.

## Consequences

Cleanup is narrow, auditable, and portable through the public ProcessKit API. A core
capability gap must be requested upstream rather than patched with an unsafe local
fallback. Diagnostics may mention unrelated lookalike processes, but never act on
them.
