# processkit-cli

`processkit-cli` is a standalone, cross-platform command runner built on the
public API of [`processkit`](https://crates.io/crates/processkit). It runs one
program inside ProcessKit's kernel-backed containment boundary and reports the
run lifecycle without requiring Python or a development virtual environment.

The CLI ensures a completed or failed command cannot leave descendants behind.
It never kills processes by name; cleanup is restricted to the current run's
ProcessKit container.

The project owns the versioned JSONL event contract used by runner clients and
future adapters, including `processkit-py`. ProcessKit-rs remains the sole
owner of containment, teardown, PID-reuse discipline, and lifecycle semantics.

For a map of the modules below, the data flow of one run, and the
control-plane contour, see [the architecture overview](docs/architecture.md).

## Installation

`processkit-cli` ships as a single self-contained binary — running it needs
neither a source build nor a Python or development virtualenv, which is the whole
point of the project.

### Prebuilt binaries (recommended)

Every release attaches a prebuilt archive per platform to its
[GitHub Release](https://github.com/ZelAnton/ProcessKit-CLI/releases). Download
the archive for your platform, extract the single `processkit-cli` binary
(`processkit-cli.exe` on Windows), and put it on your `PATH`:

```sh
# Linux x86_64 (glibc), for the vX.Y.Z release:
curl -sSL -o processkit-cli.tar.gz \
  https://github.com/ZelAnton/ProcessKit-CLI/releases/download/vX.Y.Z/processkit-cli-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
tar -xzf processkit-cli.tar.gz
```

Archives are named `processkit-cli-v<version>-<target-triple>.<ext>` — `.tar.gz`
for Linux and macOS, `.zip` for Windows.

### From crates.io

The prebuilt binaries do not replace `cargo install` — building from source stays
a first-class path:

```sh
cargo install processkit-cli
```

### Platform matrix

Prebuilt binaries are published for the targets below. The **Container mechanism**
column is the kernel-backed containment the runner *actually* reports in the
`run_started` event's `mechanism` field on that platform (see the
[JSONL event schema](#jsonl-event-schema)) — not a generic promise:

| Platform | Target triple | Container mechanism |
| --- | --- | --- |
| Windows x86_64 | `x86_64-pc-windows-msvc` | Job Object (`job_object`) |
| Windows aarch64 | `aarch64-pc-windows-msvc` | Job Object (`job_object`) |
| Linux x86_64 (glibc) | `x86_64-unknown-linux-gnu` | cgroup v2 (`cgroup_v2`) |
| Linux aarch64 (glibc) | `aarch64-unknown-linux-gnu` | cgroup v2 (`cgroup_v2`) |
| Linux x86_64 (musl, static) | `x86_64-unknown-linux-musl` | cgroup v2 (`cgroup_v2`) |
| macOS x86_64 (Intel) | `x86_64-apple-darwin` | process group (`process_group`) |
| macOS aarch64 (Apple Silicon) | `aarch64-apple-darwin` | process group (`process_group`) |

The **musl** build links libc statically, so it runs on minimal, glibc-less
container images (Alpine, distroless) as a single dependency-free file. It is
shipped **alongside** the glibc Linux build, not as a replacement.

The three mechanisms are not equally strong, and the runner reports which one is
in force rather than papering over the difference:

- **Windows — Job Object.** The whole process tree is reaped even if the runner
  itself dies abruptly (the OS closes the Job on its last handle). This is the
  strongest guarantee.
- **Linux — cgroup v2.** The run's cgroup bounds the entire tree and teardown
  reaps every member. It requires cgroup v2 (the unified hierarchy — standard on
  modern distros). Where cgroup v2 delegation is unavailable, the runner honestly
  falls back to the POSIX **process-group** mechanism below and reports
  `process_group`; it never claims a cgroup it did not get. If the runner itself
  dies abruptly, the enabled parent-death signal kills the direct child, but the
  cgroup persists and does not automatically kill grandchildren.
- **macOS and other Unix — process group.** Teardown signals the process group;
  a descendant that deliberately leaves it (`setsid` / double-fork) can escape,
  and a just-exited child may still be listed in the post-kill snapshot. The
  current ProcessKit API provides no parent-death cleanup on these targets.

Every `run_started` event reports this separate abrupt-owner-death contract as
`abrupt_cleanup`: `whole_tree` on Windows, `direct_child_only` on Linux, and
`none` on macOS/other Unix. Normal completion, timeout, and Ctrl-C still run the
owned container's ordinary teardown path on every supported platform.

## Command interface

```text
processkit-cli run     [--run-id <id>] [--cwd <dir>] --jsonl <events.jsonl>
                       [--create-no-window] [--timeout <duration>]
                       [--grace <duration>] [--capture-dir <dir>] [--argv-raw]
                       [--env-clear] [--env-remove <KEY>]... [--env <KEY=VALUE>]...
                       -- <program> <args...>
processkit-cli inspect --run-id <id> --json
processkit-cli cancel  --run-id <id>
processkit-cli kill    --run-id <id>
processkit-cli list    [--json]
processkit-cli prune   [--json]
processkit-cli probe   --json [--require-schema-version <N>]
                       [--require-exit-code-band <start>-<end>]
                       [--require-surface <token>]...
```

The command is intentionally shell-free: `run` executes `<program> <args...>`
directly, with no shell to expand or re-interpret anything after `--`. Child
stdout and stderr are echoed through unchanged; runner diagnostics and JSONL
events never use stdout.

`probe` is a side-effect-free preflight: it prints this binary's compatibility
surface (version, `schema_version`, exit-code band, and CLI surface tokens) as one
JSON line and — with `--require-*` — verifies it, so a consumer can confirm a
runner is usable **before** launching a payload. It spawns no child and touches no
registry or container.

Live output is **pipe + echo, not a real inherited terminal**: ProcessKit reads
the child's stdout/stderr through pipes and this runner re-emits them onto its
own stdout/stderr. A deliberate, honest consequence is that the child sees **no
TTY**, so terminal-dependent behavior can degrade — colors, progress bars, and
other cursor tricks may render as plain, line-oriented text. A true PTY is not
implemented here (PTY support is deferred in the core crate).

`inspect`, `cancel`, and `kill` all communicate with a live `run` process over the
same local IPC control plane, addressing it by `run_id` through the per-user registry
— never by PID. `inspect` is read-only; `cancel` ends the run through the *same*
soft-stop → grace → hard-kill teardown a `Ctrl-C` uses (exiting the run with the
reserved code `108`), and `kill` hard-kills the whole tree immediately with no grace
(code `109`). The scope of a cancel/kill is only the target run's ProcessKit
container — never processes matched by executable name. Both outcomes are also written
to the run's JSONL stream (a `cancelled` / `killed` event and a terminal
`runner_exit`), so an external observer reading the event file sees the command too,
not just the control client. If the runner has already died, the registry entry is
stale rather than an invitation to address processes by PID, and a cancel/kill against
it is a bounded `CONTROL` (103) failure — never a hang. Cleanup after an abrupt runner
death follows the platform-specific `abrupt_cleanup` guarantee above; only Windows
currently guarantees the whole tree.

`list` is the discovery counterpart to `inspect`/`cancel`/`kill`: it scans the same
per-user registry and prints every entry it finds — `run_id`, health (`live`/
`stale`), `started_at`, and `endpoint` — for an operator or orchestrator that has
lost (or never had) a `run_id`. It is read-only and never connects to any runner's
control transport, so it has none of their unreachable-run failure modes. Without
`--json` it prints a human-readable table (`no runs registered` for an empty
registry); with `--json` it prints one JSON object per entry, one per line. An
empty registry is not an error — `list` exits `0` either way — and a stale entry is
listed rather than hidden, since surfacing a leftover from an abruptly-died runner
is exactly the point. See [`docs/registry.md`](docs/registry.md), "Discovery —
`list`".

`list` shows those stale leftovers; `prune` removes them. It reaps every registry
entry it can **confirm** is stale — the `.json`/`.lock` pair a runner that died
abruptly never cleaned up — and leaves every other entry alone. The safety rule is
strict and one-directional: prune deletes **only** an entry whose liveness probe
*succeeded and reported stale*. A **live** entry is never touched; an entry whose
probe merely **failed** (its lock file could not be opened, so its liveness is
unknown, not confirmed dead) is **left in place**, on every repeated run; and no
entry is ever addressed by PID — a stale record is reaped through the path the scan
already found, so PID reuse can never misdirect a deletion. A confirmed-stale entry
is removed while its lock is still held, so a second concurrent prune cannot race on
the same files. Without `--json` it prints a one-line summary (`no stale entries to
prune` when there was nothing to do); with `--json` it prints a single JSON object
`{"pruned":N,"live":N,"unprobed":N}`. An empty or never-created registry is not an
error — `prune` reports a zero tally and exits `0`, and pruning a missing registry
does not create it. See [`docs/registry.md`](docs/registry.md), "Reaping — `prune`".

## Exit codes

The runner's exit code **is** the child's exit code; the runner's own failures
(bad arguments, spawn failure, backend error) and the four runner-*imposed*
endings — a `--timeout` (`106`), a `Ctrl-C` cancel (`107`), a control-plane `cancel`
(`108`), and a control-plane `kill` (`109`) — use a distinct, reserved code band so
they can never be mistaken for a child result, and so each ending is tellable from the
others by code alone. This is part of the project's compatibility surface — see
[the exit-code contract](docs/exit-codes.md). The preflight `probe` adds one code
to the reserved band — `PROBE_INCOMPATIBLE` (`110`) — for an incompatible launch
candidate.

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
Windows deployments that want to suppress a stray `conhost` window for the child
pass `--create-no-window` explicitly.

## Environment

By default the child inherits the runner's own environment unchanged, exactly as
launching it directly would. Three flags give control over that, mapping straight
onto `processkit::Command`'s own environment builder (`env`/`env_remove`/
`env_clear`) — the runner never reimplements this logic itself:

- `--env-clear` clears the child's entire inherited environment, so it starts
  from an empty slate rather than the runner's own environment.
- `--env-remove <KEY>` (repeatable) removes one inherited variable by name.
- `--env <KEY=VALUE>` (repeatable) sets one variable for the child, splitting on
  the *first* `=` (so a value that itself contains `=` is preserved verbatim).

**Applied order: clear, then remove, then set** — regardless of the order the
flags are given on the command line. Concretely: `--env-clear` first empties the
slate (or is skipped if absent), `--env-remove` then strips any of the remaining
inherited variables it names, and `--env` is applied last, so an explicit `--env`
always wins over an `--env-remove` of the same key. This is the intuitive "what I
set is what I get" outcome: combining `--env-clear` with an `--env` for the same
key still leaves that key set (`--env` fills the slate back in after the clear),
and combining `--env-remove KEY` with `--env KEY=VALUE` always sets `KEY=VALUE`
regardless of which flag was written first on the command line.

## Bounded output capture

`--capture-dir <dir>` records a transcript of the child's output to files —
`<dir>/stdout.log` and `<dir>/stderr.log` — **alongside** the live echo, which is
unchanged: the same output still streams to the runner's own stdout/stderr. The
child's stdout and stderr are captured to separate files, never interleaved.

The capture is **bounded**. ProcessKit's line pump drains the child's pipes under a
byte-capped [`OutputBufferPolicy`](https://docs.rs/processkit) — so the runner
writes no draining or volume-limiting of its own — and each file is held to a
per-stream ceiling. For each stream the runner records, in the `output_captured`
JSONL event (see [the schema](docs/schema.md)):

- a **full byte counter** — every byte the stream produced, so it stays honest even
  when the file was clipped;
- a **SHA-256** of the bytes written to the file (the same one-way digest used for
  the argv fingerprint), so a consumer can verify the file it holds; and
- an **explicit truncation flag** — set when the stream outran the ceiling, so
  "captured in full" is told from "clipped at the limit" by the flag, not by
  guessing from the file's size.

Without `--capture-dir`, nothing changes: no capture files, no `output_captured`
event, and the event stream is byte-for-byte identical to a plain run.

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
launch and container errors, an `output_captured` event when `--capture-dir` is
set (see "Bounded output capture"), and a terminal `runner_exit` that preserves the
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
- Machine-readable JSON Schema (draft 2020-12) for the same contract:
  [`fixtures/schema/v1/schema.json`](fixtures/schema/v1/schema.json).

## Status

`run` is implemented: it spawns the child into a ProcessKit container the runner
owns, echoes the child's output live, forwards the child's exit code exactly,
enforces `--timeout`, `--grace`, and `Ctrl-C` cancellation with a guaranteed
teardown of the whole tree (see "Timeouts, cancel, and grace"), and writes the
versioned JSONL event stream to `--jsonl` (see "JSONL event schema"). `--run-id`
and `--argv-raw` are consumed by that stream, and `--capture-dir` records a bounded
stdout/stderr transcript with per-stream byte counts, hashes, and truncation flags
(see "Bounded output capture"). `inspect`, `cancel`, and `kill` all reach live runs
through the local control plane: `inspect` prints a snapshot, `cancel` ends a run
gracefully (its shared soft-stop → grace → hard-kill teardown, exit `108`), and `kill`
force-kills the whole tree immediately (exit `109`) — each a distinguishable outcome in
the JSONL stream and by exit code, and each a bounded `CONTROL` (103) failure when the
run cannot be reached. `list` scans the same registry read-only and prints every
entry, live or stale, as a table or (with `--json`) as JSON Lines — the discovery
counterpart for a caller that has lost or never had a `run_id`. `probe` reports and
verifies the binary's compatibility surface for a consumer's fail-closed launcher
preflight, with no side effects, exiting `PROBE_INCOMPATIBLE` (110) on an
incompatible candidate. See [the roadmap](docs/ROADMAP.md) for the intended
delivery order.

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
