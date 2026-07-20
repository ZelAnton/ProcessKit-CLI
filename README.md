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
(bad arguments, spawn failure, backend error) and the two runner-*imposed*
endings — a `--timeout` (`106`) and a `Ctrl-C` cancel (`107`) — use a distinct,
reserved code band so they can never be mistaken for a child result. This is part
of the project's compatibility surface — see
[the exit-code contract](docs/exit-codes.md).

## Timeouts, cancel, and grace

`run` bounds a run two ways, and both end in the **same** teardown path:

- `--timeout <duration>` is a hard deadline for the whole run. When it elapses the
  runner ends the run and exits with the reserved `TIMEOUT` code (`106`) — never
  the child's own code, because the child did not choose to stop.
- **`Ctrl-C`** cancels a run in progress. The runner ends it and exits with the
  reserved `CANCELLED` code (`107`), distinct from a timeout and from any child
  code, so "I interrupted it" is never confused with "it ran too long" or with a
  child that merely returned non-zero.

`--grace <duration>` sets the pause between the *soft* stop and the *hard* kill on
both paths: the runner first asks the process tree to stop, waits up to the grace
window, and only then hard-kills whatever remains. The hard kill is always the
owning container's kernel-backed **kill-on-drop** (a Windows Job Object close, a
Linux cgroup / POSIX-group teardown), so the whole tree — including any leaked
grandchild — is reaped on every ending.

Durations use a small grammar: a non-negative integer with an optional unit —
`ms`, `s` (the default), `m`, or `h` (e.g. `30`, `500ms`, `5s`, `2m`, `1h`). A
malformed value is a usage error (exit `100`), not a surprise at runtime.

**Honest degradation on Windows.** The soft-stop tier is not yet implemented in the
ProcessKit kernel on Windows (tracked in ProcessKit-rs's backlog). Until it lands,
the runner sends **no** soft signal on Windows: the grace window still elapses, and
then the Job Object is killed atomically. The runner never pretends a graceful
soft-terminate happened when it could not — the stderr line for a Windows
timeout/cancel says plainly that the tree was hard-killed via the Job Object after
the grace delay. On Unix the soft stop is a real `SIGTERM` to the tree.

The machine-readable form of these outcomes is the `timeout` / `cancelled` event
(and the terminal `runner_exit`) in the versioned JSONL stream written to
`--jsonl` — see [the JSONL event schema](#jsonl-event-schema) — alongside the exit
code and the stderr message.

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

## JSONL event schema

`run` writes a versioned stream of **JSONL lifecycle events** to the file named by
`--jsonl` — one JSON object per line, each carrying a `schema_version`, and
**never** to stdout (the child's streams stay pristine). This repository owns that
contract as a public API: adapters such as the processkit-py CLI pin
`schema_version` and reimplement the shapes, so it is versioned, documented, and
golden-tested.

The stream covers the run lifecycle: `run_started` (run id, root PID, containment
mechanism, working directory), `members_snapshot`, `root_exited`, the
`cleanup_started` / `cleanup_finished` teardown pair, `timeout` / `cancelled`,
launch and container errors, and a terminal `runner_exit` that preserves the
child's own code even when the runner itself fails — so a child's code is never
lost or aliased.

The command line is **redacted by default** (`argv` is recorded only under
`--argv-raw`); in its place a one-way SHA-256 fingerprint of argv (`argv_sha256`)
and, for recognized worker shapes, a categorical `hint` are recorded on every run —
neither can reveal the command line. Member snapshots are PID-only today, with the
richer per-member fields declared but absent until ProcessKit-rs ships them.

- Normative field reference: [`docs/schema.md`](docs/schema.md).
- Golden sample stream for adapters:
  [`fixtures/schema/v1/events.jsonl`](fixtures/schema/v1/events.jsonl).

## Status

`run` is implemented: it spawns the child into a ProcessKit container the runner
owns, echoes the child's output live, forwards the child's exit code exactly,
enforces `--timeout`, `--grace`, and `Ctrl-C` cancellation with a guaranteed
teardown of the whole tree (see "Timeouts, cancel, and grace"), and writes the
versioned JSONL event stream to `--jsonl` (see "JSONL event schema"). `--run-id`
and `--argv-raw` are consumed by that stream. The control-plane subcommands
(`inspect`, `cancel`, `kill`) are not implemented yet — those still report a
runner-range "not implemented" error — and `--capture-dir` is parsed but not yet
consumed (bounded diagnostic capture is a later task). See
[the roadmap](docs/ROADMAP.md) for the intended delivery order.

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
- Raw command arguments are opt-in only; the default diagnostics contract uses
  redaction-safe hashes and worker hints.
