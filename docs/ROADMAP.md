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
memory/active-process limit): today's `processkit` 3.1.0, the version this
repository resolves from crates.io (`Cargo.lock`), exposes no portable
post-spawn evidence primitive for that case, so a live limit kill remains
indistinguishable from the child failing on its own.

The cross-repo request for that primitive
(`msg-send-401e87d4625e22218e50a11de4a7f122`) has since been answered and
implemented upstream: `ProcessGroup::limit_evidence()` landed on ProcessKit-rs
`main` (ProcessKit-rs task T-243) with a three-valued
`LimitVerdict::{Tripped, NotTripped, Unknown}` per axis — never a boolean —
so a future JSONL surface for this must represent "no authoritative evidence"
as its own state and must never collapse it into "did not fire" (see
[`docs/resource-limits.md`](resource-limits.md#applied-limit-versus-observed-limit-hit)
for the platform-by-platform breakdown and the read-before-drop constraint on
where a future reader could sit). It is **not yet in a published release** —
the latest tag remains v3.1.0 — so nothing is consumable today and this
dependency stays open in practice. This roadmap does not bump or repoint the
dependency; the scheduling trigger is the upstream release notification
arriving in this project's inbox, at which point wiring `limit_evidence()`
into the JSONL stream — an additive schema change, exact shape (a `limit_hit`
discriminator field vs. a separate event) to be decided when it is planned —
gets scheduled. Note that even once wired, this closes the gap on Linux
cgroup v2 only: Windows Job Object and POSIX process groups (macOS, the BSDs,
the Linux process-group fallback) report `Unknown` as a measured result, not
an unfinished one, so runtime limit attribution will not become available on
Windows despite it being a first-class platform for this CLI.
