# Roadmap

## 1. Runnable containment shell

Implement `processkit-cli run` using the public `processkit` API: shell-free
spawn, ProcessGroup ownership, stdout/stderr echo, child exit-code fidelity,
and Ctrl-C teardown. Runner failures will use a documented non-child exit-code
range.

## 2. JSONL schema v1

Publish the normative, versioned event schema and golden fixtures. It will
cover run start, member snapshots, root exit, cleanup, cancellation/timeouts,
runner errors, and runner exit. Events go to a file, never stdout; argv is
redacted by default.

## 3. Bounded diagnostic capture

Add optional stdout/stderr capture files with byte counts, hashes, and explicit
truncation metadata while preserving live echoed output.

## 4. Live-run control plane

Add a per-user registry and local IPC so `inspect`, `cancel`, and `kill` reach
the live runner process. Stale entries and runner death must be detectable.

## 5. End-to-end proof

Exercise the shipped binary against leaked descendants, nonzero roots, abrupt
runner death, inherited pipe handles, concurrent runs, nested Windows Jobs,
PID reuse, and Ctrl-C. The Windows worker scenario will cover the
`MSBuild.dll /nodemode:1 /nodeReuse:true` shape when available.

## 6. Distribution and Orchestra migration

Publish release binaries for Windows, Linux, and macOS (with a musl Linux
variant), support `cargo install`, and coordinate Orchestra's fail-closed
binary launcher contract.

## Dependencies on ProcessKit-rs

The CLI will consume, rather than duplicate, the core's forthcoming
`ProcessGroup::members_info()` snapshots and Windows graceful shutdown support.
Until then, member snapshots may be PID-only and Windows cancellation must
report its hard-kill fallback honestly.

Whole-tree cleanup after an abrupt runner death is also a core dependency on
Unix. The current public primitive kills only the direct child on Linux and is a
no-op on macOS/BSD; cgroups and process groups do not disappear with their owner.
Until ProcessKit exposes an additive, identity-safe whole-tree owner-death
primitive, the CLI reports `direct_child_only` or `none` in `run_started` and
does not claim the Windows guarantee on those platforms. The core work is
tracked as ProcessKit-rs task T-151 and must include cross-platform abrupt-death
proof before this contract can be strengthened.
