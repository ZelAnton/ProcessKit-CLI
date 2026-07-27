![ProcessKit: async child-process management with a kernel-backed no-orphan guarantee](processkit-cover.png)

[![Crates.io](https://img.shields.io/crates/v/processkit-cli.svg)](https://crates.io/crates/processkit-cli)
[![Docs](https://img.shields.io/badge/docs-GitHub%20Pages-0A7BBB?logo=github)](https://zelanton.github.io/ProcessKit-CLI/)
[![CI](https://github.com/ZelAnton/ProcessKit-CLI/actions/workflows/ci.yml/badge.svg)](https://github.com/ZelAnton/ProcessKit-CLI/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/LICENSE)

# processkit-cli

`processkit-cli` is the standalone command runner for ProcessKit. It executes
one shell-free command inside the public [`processkit`](https://crates.io/crates/processkit)
crate's containment boundary, preserves the child's exit code, and records a
versioned JSONL lifecycle stream without requiring a Rust or Python runtime at
the call site.

The runner owns the command-line and event contracts. ProcessKit-rs remains the
single source of truth for containment, teardown, PID-reuse discipline, and
platform lifecycle behavior. The cover above represents the wider ProcessKit
family; this CLI deliberately focuses on one contained run and its control
plane rather than exposing the core crate's pipelines, retries, or scheduling
APIs.

## Install

Download a platform archive from the
[latest GitHub release](https://github.com/ZelAnton/ProcessKit-CLI/releases/latest),
or build and install from crates.io:

```sh
cargo install processkit-cli
```

Release archives contain the binary, shell completions, man pages, the JSON
Schema, a SHA-256 checksum, and a signed build-provenance attestation.

## Quick start

Run a command directly, with no shell between the runner and the program:

```sh
processkit-cli run --jsonl events.jsonl -- cargo --version
```

Child stdout and stderr pass through unchanged. Lifecycle events go only to
`events.jsonl`, so an adapter can consume them without parsing or contaminating
the child's output.

Use a stable run id when another process needs to inspect or stop the live run:

```sh
processkit-cli run --run-id build-42 --jsonl events.jsonl -- cargo test
processkit-cli inspect --run-id build-42 --json
processkit-cli cancel --run-id build-42
```

The control commands address a per-user registry entry and a live local IPC
endpoint, never an operating-system PID. A reused PID therefore cannot retarget
an old command at an unrelated process.

## What the runner guarantees

- **One owned process tree.** Normal completion, timeout, cancellation, and
  runner errors tear down the current run's ProcessKit container. Cleanup never
  searches by executable name.
- **Exit-code fidelity.** A normal child exit is returned unchanged. Runner
  failures occupy the documented `100`-`119` band and also emit `runner_exit`,
  so a child code is never silently aliased.
- **Separated streams.** Child output stays on stdout/stderr; JSONL events stay
  in `--jsonl`; runner diagnostics never enter child stdout.
- **Redacted diagnostics.** Events contain a SHA-256 argv fingerprint and a
  classified worker hint by default. Raw arguments require `--argv-raw`.
- **Bounded capture.** `--capture-dir` tees stdout and stderr into separate,
  size-capped transcripts with byte counts, hashes, and truncation metadata.
- **Honest platform reporting.** `run_started` records the active containment
  mechanism and the real abrupt-runner-death cleanup guarantee rather than
  presenting every operating system as equivalent.

## Command surface

| Command | Purpose |
| --- | --- |
| `run` | Start one contained, shell-free command and write lifecycle JSONL. |
| `inspect` | Snapshot a live run and its current members. |
| `cancel` | Request soft stop, wait through the grace window, then hard-kill survivors. |
| `kill` | Hard-kill the run's whole container immediately. |
| `wait` | Wait for one run, or a snapshot of all live runs, to finish. |
| `list` | Discover live, stale, and unprobed registry entries. |
| `prune` | Remove only entries confirmed stale. |
| `probe` | Verify the binary's versioned compatibility surface before launch. |

Run `processkit-cli <command> --help` for the complete flag set. The
[integration guide](integration.md) shows a fail-closed adapter workflow from
preflight through cleanup.

## Platform behavior

| Platform | Preferred mechanism | Abrupt runner death |
| --- | --- | --- |
| Windows | Job Object | Whole tree is reaped by kernel kill-on-close. |
| Linux | cgroup v2, with process-group fallback | Direct child only when parent-death signaling is available. |
| macOS / other Unix | POSIX process group | No automatic whole-tree guarantee after an uncatchable runner death. |

Every ordinary teardown still uses the active container on every supported
platform. The last column is intentionally narrower: it describes only a crash,
`SIGKILL`, or comparable event that prevents the runner from executing its own
cleanup path. See the [architecture](architecture.md) and
[troubleshooting guide](troubleshooting.md) for the exact caveats.

## Documentation

| Guide | Covers |
| --- | --- |
| [Integration guide](integration.md) | Probe, launch, event consumption, supervision, and housekeeping for adapters. |
| [Control plane](control-plane.md) | IPC transport, inspect/cancel/kill semantics, and safe targeting. |
| [Run registry](registry.md) | Per-user records, liveness probing, ambiguity, waiting, and pruning. |
| [JSONL event schema](schema.md) | The normative `schema_version = 1` contract and golden fixtures. |
| [Exit-code contract](exit-codes.md) | Child-code fidelity and the reserved runner failure band. |
| [Troubleshooting](troubleshooting.md) | Symptom-to-cause diagnosis for operators and CI. |
| [Threat model](threat-model.md) | Trusted boundaries, hostile inputs, local IPC, and supply chain. |
| [Architecture](architecture.md) | Module map and the data flow of one run. |

Source, release history, and contribution guidance live in the
[GitHub repository](https://github.com/ZelAnton/ProcessKit-CLI).
