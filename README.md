# processkit-cli

`processkit-cli` is a standalone, cross-platform command runner built on the
public API of [`processkit`](https://crates.io/crates/processkit). It runs one
program inside ProcessKit's kernel-backed containment boundary and reports the
run lifecycle without requiring Python or a development virtual environment.

Its primary consumer is Orchestra: an agent orchestrator that must ensure a
completed or failed command cannot leave descendants behind, including workers
such as `MSBuild.dll /nodemode:1 /nodeReuse:true` on Windows. The CLI never
kills processes by name; cleanup is restricted to the current run's ProcessKit
container.

The project owns the versioned JSONL event contract used by runner clients and
future adapters, including `processkit-py`. ProcessKit-rs remains the sole
owner of containment, teardown, PID-reuse discipline, and lifecycle semantics.

## Planned interface

```text
processkit-cli run     [--run-id <id>] [--cwd <dir>] --jsonl <events.jsonl>
                       [--create-no-window] [--timeout <duration>]
                       [--grace <duration>] [--capture-dir <dir>] [--argv-raw]
                       -- <program> <args...>
processkit-cli inspect --run-id <id> --json
processkit-cli cancel  --run-id <id>
processkit-cli kill    --run-id <id>
```

The command is intentionally shell-free. Child stdout and stderr will be
echoed unchanged; runner diagnostics and JSONL events never use stdout.

`inspect`, `cancel`, and `kill` will communicate with a live `run` process over
local IPC. If the runner dies, ProcessKit's kill-on-drop containment ends that
run's tree; the registry entry is then stale rather than an invitation to
address processes by PID.

## Status

Repository scaffolding is complete; the runner has not been implemented yet.
See [the roadmap](docs/ROADMAP.md) for the intended delivery order.

## Development

```powershell
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

## Safety boundaries

- No reimplementation of ProcessKit containment or lifecycle behavior.
- No shell mode, PTY support, or global cleanup by executable/process name.
- Raw command arguments are opt-in only; the default diagnostics contract will
  use redaction-safe hashes and worker hints.
