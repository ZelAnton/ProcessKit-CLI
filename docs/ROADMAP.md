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

The CLI will consume, rather than duplicate, the core's forthcoming
`ProcessGroup::members_info()` snapshots and Windows graceful shutdown support.
Until then, member snapshots may be PID-only and Windows cancellation must
report its hard-kill fallback honestly.

Whole-tree cleanup after an abrupt runner death is also a core dependency on
Unix. The current public primitive kills only the direct child on Linux and is a
no-op on macOS/BSD; cgroups and process groups do not disappear with their owner.
Until ProcessKit exposes an additive, identity-safe whole-tree owner-death
primitive, the CLI reports `direct_child_only` or `none` in `run_started` and
does not claim the Windows guarantee on those platforms. Any stronger contract
requires additive, identity-safe ProcessKit-rs support and cross-platform
abrupt-death proof.
