# Roadmap

## Delivered in v0.2.0

1. **Runnable containment shell.** `processkit-cli run` executes one shell-free
   command through the public `processkit` API, echoes child stdout/stderr, and
   preserves the child exit code. Timeouts and cancellation use a distinct,
   documented runner-owned exit-code band.
2. **JSONL schema v1.** The normative event schema and golden fixtures cover
   lifecycle events, cleanup, runner failures, and terminal exit. Events are
   written to `--jsonl`, never stdout, and argv is redacted by default.
3. **Bounded diagnostic capture.** `--capture-dir` writes separate bounded
   stdout/stderr transcripts with full byte counts, hashes, and truncation
   metadata while preserving live echoed output.
4. **Live-run control plane.** The per-user registry and local IPC back
   `inspect`, `cancel`, `kill`, `list`, and `prune`; stale entries are visible
   and safely reaped without addressing a process by PID.
5. **End-to-end proof.** Through-the-binary tests cover leaked descendants,
   nonzero roots, inherited pipe handles, concurrent runs, control-plane
   cancellation, and platform-specific teardown behavior. The heavier `e2e`
   tier additionally covers abrupt runner death, nested Windows Jobs, PID reuse,
   and Ctrl-C.
6. **Distribution.** Releases publish six prebuilt archives: Windows x86_64 and
   aarch64, Linux x86_64 glibc and musl plus aarch64 glibc, and Apple Silicon
   macOS. Source installation remains available through `cargo install`.

## Delivered in v0.2.1

1. **Explicit stdin sources.** `--inherit-stdin` shares the runner's input
   handle with the child, while `--stdin-file <file>` streams a checked file
   through ProcessKit and closes stdin at EOF. Closed/null stdin remains the
   safe default.

## Current development

1. **Interactive inherited stdio.** `--inherit-stdio` passes stdin, stdout, and
   stderr directly to the child, preserving an existing console or terminal
   while retaining ProcessKit containment, lifecycle JSONL, the control plane,
   cleanup, and exit-code fidelity. The default remains pipe + echo; transcript
   capture and no-console mode intentionally conflict with direct inheritance.
2. **Cross-platform terminal proof.** Through-the-binary tests cover piped I/O,
   a real Windows console, and a POSIX pseudo-terminal, including input,
   terminal detection, JSONL completion, and descendant cleanup.

## Remaining ProcessKit-rs dependencies

The processkit 3 graceful-shutdown contract is now fully consumed. Windows console
children can opt into `CTRL_BREAK` with `--windows-graceful-ctrl-break`; every
runner-imposed graceful ending probes `ProcessGroup::soft_stop_scope()` before the
attempt and records the resulting `ShutdownReport` in
`cleanup_finished.shutdown`. `ProcessGroup::members_info()` is likewise consumed
for `members_snapshot`/`inspect` enrichment (see
[`docs/schema.md`](schema.md), "Enriched member fields").

Whole-tree cleanup after an abrupt runner death is also a core dependency on
Unix. The current public primitive kills only the direct child on Linux and is a
no-op on macOS/BSD; cgroups and process groups do not disappear with their owner.
Until ProcessKit exposes an additive, identity-safe whole-tree owner-death
primitive, the CLI reports `direct_child_only` or `none` in `run_started` and
does not claim the Windows guarantee on those platforms. Any stronger contract
requires additive, identity-safe ProcessKit-rs support and cross-platform
abrupt-death proof.

Runtime resource-limit attribution is also a core dependency. `limit_hit`
(see [`docs/schema.md`](schema.md#limit_hit)) today covers only the pre-spawn
"could not be applied" branch — a requested cap the platform has no container
for, or a Linux cgroup v2 whose controllers can't be enabled. It does not, and
currently cannot, cover a cap that *was* applied and then actually fired during
the run (a Linux cgroup OOM-kill or `pids` fork refusal, a Windows Job Object
memory/active-process limit): ProcessKit's public API exposes no portable
post-spawn evidence primitive for that case (no `memory.events`/`pids.events`-
style readback, no Job Object notification/query), so a live limit kill is
today indistinguishable from the child failing on its own. A cross-repo
request for an additive, identity-safe post-spawn evidence primitive has been
sent to ProcessKit-rs (`msg-send-401e87d4625e22218e50a11de4a7f122`). Until it
ships, publishing that attribution in the JSONL stream — an additive schema
change, exact shape (a `limit_hit` discriminator field vs. a separate event)
to be decided when it is planned — remains a future roadmap item with no
committed timeline.
