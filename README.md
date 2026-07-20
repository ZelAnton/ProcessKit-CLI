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

The command is intentionally shell-free: `run` executes `<program> <args...>`
directly, with no shell to expand or re-interpret anything after `--`. Child
stdout and stderr are echoed through unchanged; runner diagnostics and JSONL
events never use stdout.

Live output is **pipe + echo, not a real inherited terminal**: ProcessKit reads
the child's stdout/stderr through pipes and this runner re-emits them onto its
own stdout/stderr. A deliberate, honest consequence is that the child sees **no
TTY**, so terminal-dependent behavior can degrade — colors, progress bars, and
other cursor tricks may render as plain, line-oriented text. A true PTY is not
implemented here (PTY support is deferred in the core crate).

`inspect`, `cancel`, and `kill` will communicate with a live `run` process over
local IPC. If the runner dies, ProcessKit's kill-on-drop containment ends that
run's tree; the registry entry is then stale rather than an invitation to
address processes by PID.

## Exit codes

The runner's exit code **is** the child's exit code; the runner's own failures
(bad arguments, spawn failure, backend error) use a distinct, reserved code band
so they can never be mistaken for a child result. This is part of the project's
compatibility surface — see [the exit-code contract](docs/exit-codes.md).

## Windows console

`--create-no-window` maps directly onto ProcessKit's
`Command::create_no_window()` (the `CREATE_NO_WINDOW` creation flag on Windows; a
no-op elsewhere). **It defaults to off.** A bare `run` should behave as much like
launching the child directly as possible, so the runner does not force the flag —
doing so unconditionally would diverge from a direct launch and could hide a
child that legitimately wants its own console. The runner itself never allocates
a console, so it spawns no extra console host on its own account. Headless
Windows deployments (such as Orchestra) that want to suppress a stray `conhost`
window for the child pass `--create-no-window` explicitly.

## Status

`run` is implemented: it spawns the child into a ProcessKit container the runner
owns, echoes the child's output live, and forwards the child's exit code exactly.
The control-plane subcommands (`inspect`, `cancel`, `kill`) and the JSONL event
stream are not implemented yet — those subcommands still report a runner-range
"not implemented" error, and `run`'s `--jsonl`, `--timeout`, `--grace`, and
related flags are parsed but not yet consumed. See [the roadmap](docs/ROADMAP.md)
for the intended delivery order.

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
