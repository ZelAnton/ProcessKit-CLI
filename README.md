![ProcessKit: async child-process management with a kernel-backed no-orphan guarantee](docs/processkit-cover.png)

# processkit-cli

[![Docs](https://img.shields.io/badge/docs-GitHub%20Pages-0A7BBB?logo=github)](https://zelanton.github.io/ProcessKit-CLI/)

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
For a consumer/adapter walkthrough — preflight, launching a run, reading the
JSONL stream, supervision, and housekeeping — see
[the integration guide](docs/integration.md). For symptom-to-cause diagnosis
of common operator issues (CI resource-limit failures, a stray Windows
console window, ambiguous run ids, and more), see
[the troubleshooting guide](docs/troubleshooting.md).

## Installation

`processkit-cli` ships as a single self-contained binary — running it needs
neither a source build nor a Python or development virtualenv, which is the whole
point of the project.

### Prebuilt binaries (recommended)

Every release attaches a prebuilt archive per platform to its
[GitHub Release](https://github.com/ZelAnton/ProcessKit-CLI/releases), each with
a `<archive>.sha256` checksum next to it. Download the archive for your platform,
verify it against its published SHA-256 checksum, extract the single
`processkit-cli` binary (`processkit-cli.exe` on Windows), and put it on your
`PATH`:

```sh
# Linux x86_64 (glibc), for the vX.Y.Z release:
archive=processkit-cli-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
base=https://github.com/ZelAnton/ProcessKit-CLI/releases/download/vX.Y.Z
curl -sSL -O "$base/$archive"          # the archive
curl -sSL -O "$base/$archive.sha256"   # its SHA-256 checksum
sha256sum -c "$archive.sha256"         # macOS: shasum -a 256 -c "$archive.sha256"
tar -xzf "$archive"
```

Archives are named `processkit-cli-v<version>-<target-triple>.<ext>` — `.tar.gz`
for Linux and macOS, `.zip` for Windows — and each ships a matching
`<archive>.sha256`.

Every archive also carries a signed [build-provenance
attestation](https://docs.github.com/actions/security-guides/using-artifact-attestations).
For a stronger, cryptographic check that the archive was built by this
repository's release workflow (not just that its bytes are intact), verify it
with the [GitHub CLI](https://cli.github.com):

```sh
gh attestation verify "$archive" --repo ZelAnton/ProcessKit-CLI
```

### From crates.io

The prebuilt binaries do not replace `cargo install` — building from source stays
a first-class path:

```sh
cargo install processkit-cli
```

Building from source also (re)generates the shell completions and man pages
described below, in `target/assets/` — see "Shell completions and man pages".

### Shell completions and man pages

Every release archive (above) also ships a `completions/` directory (bash, zsh,
fish, PowerShell, and Elvish) and a `man/man1/` directory (one page per
subcommand, plus `processkit-cli.1`), generated straight from the CLI's own
`clap` definition by `build.rs` at build time — there is no separate,
hand-maintained copy of the flags to fall out of sync, and (unlike a
`generate-completions` subcommand would) this never adds anything to the CLI's
own runtime surface, so the fail-closed `probe --require-surface` preflight
(see "Command interface" below) never sees it. `cargo install`/`cargo build`
from source produce the same two directories under `target/assets/` — see
`build.rs`'s module doc for the full mechanism. (The release archive ships a
third directory too, `schema/` — see "JSONL event schema" below; `cargo
install`/`cargo build` do not produce it, since it is a static copy of a
tracked fixture, not `build.rs` output.)

Install the completion script for your shell, e.g.:

```sh
# bash (adjust the destination to your distro's completion directory)
install -Dm644 completions/processkit-cli.bash /etc/bash_completion.d/processkit-cli

# zsh (anywhere on $fpath)
install -Dm644 completions/_processkit-cli "$HOME/.zsh/completions/_processkit-cli"

# fish
install -Dm644 completions/processkit-cli.fish "$HOME/.config/fish/completions/processkit-cli.fish"
```

```powershell
# PowerShell — dot-source it from your $PROFILE
Copy-Item completions\_processkit-cli.ps1 $HOME\Documents\PowerShell\
Add-Content $PROFILE '. $HOME\Documents\PowerShell\_processkit-cli.ps1'
```

And the man pages, onto any directory on your `MANPATH`:

```sh
mkdir -p /usr/local/share/man/man1
cp man/man1/*.1 /usr/local/share/man/man1/
man processkit-cli-run
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
`none` on macOS/other Unix. Normal completion, timeout, and a cancel signal — a
`Ctrl-C`, on Unix a `SIGTERM`/`SIGHUP`, or on Windows a `Ctrl-Break`/console
close/logoff/system shutdown, all of which the runner catches — still run
the owned container's ordinary teardown path on every supported platform; this
tri-state applies only where the runner never gets to run it at all (a crash, a
`SIGKILL`, an outer Job Object terminate).

## Command interface

```text
processkit-cli run     [--run-id <id>] [--cwd <dir>] --jsonl <events.jsonl>
                       [--create-no-window] [--timeout <duration>]
                       [--idle-timeout <duration>]
                       [--grace <duration>] [--max-memory <size>]
                       [--max-processes <n>] [--cpu-quota <cores>]
                       [--capture-dir <dir>] [--capture-max-bytes <size>]
                       [--no-echo] [--detach] [--argv-raw]
                       [--inherit-stdio | --inherit-stdin | --stdin-file <file>]
                       [--env-clear] [--env-remove <KEY>]... [--env <KEY=VALUE>]...
                       -- <program> <args...>
processkit-cli inspect --run-id <id> [--json]
processkit-cli cancel  (--run-id <id> | --all)
processkit-cli kill    (--run-id <id> | --all)
processkit-cli wait    (--run-id <id> | --all) [--timeout <duration>]
processkit-cli list    [--json]
processkit-cli prune   [--json] [--dry-run]
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

By default, live output is **pipe + echo, not a real inherited terminal**:
ProcessKit reads the child's stdout/stderr through pipes and this runner re-emits
them onto its own stdout/stderr. The child therefore sees **no TTY**, so colors,
progress bars, and other cursor tricks may degrade. `--inherit-stdio` is the
explicit alternative for interactive commands: it gives the child the runner's
three standard handles directly, preserving an existing terminal without a pump.
It does not allocate or emulate a PTY; if the caller supplied pipes or files, the
child sees those pipes or files.

## Standard I/O

By default the child receives closed/null stdin, so a noninteractive command cannot
wait forever for bytes the runner does not own. Three mutually exclusive opt-ins
change the standard-I/O path:

- `--inherit-stdio` gives the child the runner's stdin, stdout, and stderr handles
  directly. There is no output pump, echo, tee, or transcript capture in this mode;
  the child writes to the inherited destinations itself. JSONL events still go only
  to `--jsonl`, and runner diagnostics still avoid stdout. This mode conflicts with
  `--capture-dir`, `--create-no-window`, `--inherit-stdin`, `--stdin-file`,
  `--no-echo` (there is no pump to suppress in this mode), and `--detach` (a
  detached run has no terminal to hand over).
  `probe --json` advertises the capability as `run:--inherit-stdio`.

- `--inherit-stdin` gives the child the runner's own stdin handle. It can read from
  the caller's terminal, file, or pipe directly; the runner neither mediates nor
  records those bytes.
- `--stdin-file <file>` streams the named readable file through
  `processkit::Stdin::from_file` and closes the child's stdin at EOF. Input bytes
  never appear in the child argv or lifecycle JSONL. The file is checked before a
  child is spawned; an unreadable path is a `SETUP` (111) failure.

The two input-only modes leave stdout/stderr on the default pipe-and-echo path and
do not create a PTY. Their `Ctrl-C` behavior remains runner-owned cancellation with
the documented `cancelled` event and exit code `107`.

`--no-echo` suppresses only the runner's own live retransmission of the child's
stdout/stderr — the pipe and the output pump stay exactly as they are otherwise:
`--capture-dir` still receives every byte through the same tee, `--idle-timeout`
still re-arms on every observed chunk, and the JSONL event stream is unaffected.
It is meant for an embedding orchestrator that reads results from `--jsonl`/
`--capture-dir` and finds the child's raw output — interleaved with the runner's
own — pure noise. It conflicts with `--inherit-stdio`, which runs no pump to
suppress in the first place (a parse-time error, like `--capture-dir` and
`--idle-timeout`). `probe --json` advertises the capability as `run:--no-echo`.
Without `--no-echo`, nothing changes: the live echo behaves exactly as before.
[`--detach`](#detached-runs) reuses this exact path rather than suppressing output
its own way: the detached runner is started with `--no-echo`, so a detached run's
echo behavior is this one, described once.

`--inherit-stdio` preserves native terminal signal delivery instead of pretending
to mediate it. On Windows and containment mechanisms that keep child and runner in
the same foreground group, the host may deliver `Ctrl-C` to both; when the runner's
listener observes it, the result is the normal `cancelled`/`107` outcome. On Unix
when ProcessKit uses a separate process group, the runner temporarily makes that
group foreground so terminal input works, and the terminal delivers `Ctrl-C` to the
child group; a child that exits from the signal is therefore reported as a child
exit. In every case the runner restores the original foreground group and tears
down its ProcessKit container before returning. Callers that require a deterministic
runner-owned cancellation outcome should use the control-plane `cancel` command.

`inspect`, `cancel`, and `kill` all communicate with a live `run` process over the
same local IPC control plane, addressing it by `run_id` through the per-user registry
— never by PID. `inspect` is read-only and, like `list`/`prune`, has an optional
`--json`: without it, `inspect` prints a human-readable rendering of the snapshot
(snapshot version, run id, mechanism, root pid, start time, and a member table); with
`--json` it prints the snapshot as a single JSON line, unchanged. `cancel` ends the
run through the *same* soft-stop → grace → hard-kill teardown a `Ctrl-C` uses
(exiting the run with the reserved code `108`), and `kill` hard-kills the whole tree
immediately with no grace (code `109`). By `--run-id`, the scope of a cancel/kill is
only the target run's ProcessKit container (the `--all` form below widens this to
every confirmed-live registry entry, still **never** processes matched by executable
name — that boundary holds for both forms). Both
outcomes are also written to the run's JSONL stream (a `cancelled` / `killed` event
and a terminal `runner_exit`), so an external observer reading the event file sees
the command too, not just the control client. If the runner has already died, the
registry entry is stale rather than an invitation to address processes by PID, and a
cancel/kill against it is a bounded `CONTROL` (103) failure — never a hang. Cleanup
after an abrupt runner
death follows the platform-specific `abrupt_cleanup` guarantee above; only Windows
currently guarantees the whole tree.

`cancel --all` / `kill --all` (mutually exclusive with `--run-id`, exactly one
of the two required, the same clap shape `wait --all` established) are the mutating
counterpart to `wait --all`: instead of one named run, they act on every run confirmed
live in a **snapshot** taken the moment the invocation starts. The snapshot keys each
target by its unique registry-record path and remembered endpoint, not by `run_id`, so
two concurrent records sharing one explicit id are both reached independently; the
by-`run-id` form remains an ambiguity failure. Before dispatch the client reconfirms
that exact record is still live and still advertises the remembered endpoint. A run
that registers after the
snapshot, or one that is only `unprobed` (not confirmed live) *at that instant*, is
out of scope, mirroring `wait --all`'s own asymmetry. Instead of one ack, `--all`
prints a single JSON array on stdout — one entry per snapshot target with `run_id`,
`accepted`, and `status` (`accepted`, `already_gone`, or `failed`), plus `error` only
for `failed`. `already_gone` means the runner finished after the snapshot but before
its turn: no command was acknowledged (`accepted` stays false), yet the aggregate's
desired terminal state is already reached, so this is not a failure. An empty snapshot
is likewise not an error, mirroring `prune`: an empty report and exit `0`. A partial or
full failure is never a silent `0`, though — it reuses the reserved `CONTROL` (103)
code with a summary on stderr, so `cancel --all` before `wait --all`/`prune` in a
teardown sequence cannot silently swallow a target it failed to reach. See
[`docs/control-plane.md`](docs/control-plane.md), "`cancel --all` / `kill --all`".

`wait` is the *lifetime* counterpart to those three: it blocks while the named run is
live in the registry and exits `0` as soon as it is not. It exists for a supervisor
that did **not** start the run — an adapter that restarted, a cleanup step, anything
holding only a `run_id` — and so has no child process to wait on. Like `list`/`prune`
it is registry-only: it never connects to the run's control transport, never mutates
registry state, and never ends or otherwise disturbs the run (a run whose transport
never came up is still waitable, since `wait` needs no endpoint). Because the liveness
signal is an OS advisory lock with no event to subscribe to, waiting is honest periodic
probing, not a notification. `--timeout` bounds **the wait**, not the run: when it
elapses the run is left running and `wait` exits with its own reserved code `112`,
never the run's `TIMEOUT` (`106`) — and an ambiguous `run_id` (more than one live run
under it) is the same `CONTROL` (103) refusal every other by-`run-id` command gives.
Nothing is printed on success; the exit code is the answer. One deliberate design
choice deserves a caller's attention: since a clean exit deletes its own registry
entry, an **unknown** `run_id` is indistinguishable from one that already finished and
was cleaned up, so both exit `0` — meaning a typo'd `run_id` returns success
immediately, and `wait`'s `0` must never be read as proof the run existed. See
[`docs/registry.md`](docs/registry.md), "Waiting — `wait`".

`wait --all` (mutually exclusive with `--run-id`; exactly one of the two is required)
is the aggregate counterpart: an orchestrator teardown barrier that blocks until no run
this invocation considers in scope is still live, for the typical "cancel everything,
wait for it all to be gone, then `prune`" sequence. Its target set is a **snapshot**,
fixed once at the moment `--all` starts to exactly the entries **confirmed** live right
then — an entry that is only unprobed (not confirmed live) *at that instant* is
excluded from the target set outright, and a run that registers afterward is likewise
out of scope for that invocation; re-issue `wait --all` to catch either. This is a
deliberate asymmetry with `--run-id`, which has no equivalent "excluded before
tracking begins" step of its own: a registry holding only unprobeable entries and no
confirmed-live ones makes `--all` return `0` immediately, the same as an empty
registry. Once an entry *is* in the target set, though, it stays outstanding on every
later pass for as long as its liveness cannot be re-probed, rather than being silently
dropped — the same conservative stance `--run-id` takes. See
[`docs/registry.md`](docs/registry.md), "Waiting — `wait`", "The aggregate barrier —
`wait --all`".

`list` is the discovery counterpart to `inspect`/`cancel`/`kill`: it scans the same
per-user registry and prints every entry it finds — `run_id`, health (`live`/
`stale`/`unprobed`), `started_at`, the run's worker-shape `hint` and one-way argv
fingerprint `argv_sha256`, and `endpoint` — for an operator or orchestrator
that has lost (or never had) a `run_id`. It is read-only and never connects to any
runner's control transport, so it has none of their unreachable-run failure modes.
The two command fields are what make several live runs distinguishable without
disclosing a command line: the fingerprint is equal for two entries exactly when
they are running the same command, and the hint names a recognized worker shape
(`msbuild_node_reuse`, …) or is `null`. They are the same pair the JSONL
`run_started` event carries, from the same implementation; the raw argv is never
written to a registry record under any flag, `--argv-raw` included. The table
abbreviates the fingerprint to its first 12 hex characters, `--json` carries the
full digest.
Without `--json` it prints a human-readable table (`no runs registered` for an
empty registry); with `--json` it prints one JSON object per entry, one per line. An
empty registry is not an error — `list` exits `0` either way — and a stale entry is
listed rather than hidden, since surfacing a leftover from an abruptly-died runner
is exactly the point. An entry whose liveness lock could not even be *probed*
(permission denied, a rejected symlink/reparse point) prints as `unprobed`, never
the confirmed-dead `stale`, so an operator never mistakes "could not tell" for "the
runner is gone" — the same distinction `prune --json`'s `unprobed` tally, `wait`, and
an `inspect`/`cancel`/`kill` refusal against such an entry all make. See
[`docs/registry.md`](docs/registry.md), "Discovery — `list`".

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
the same files. A separate pass applies the same confirm-before-delete rule to
**orphaned lock files** — a lone `.lock` with no `.json` sibling, which the pass
above can never see — and reaps those too, tallied separately since it deletes one
file, not a pair. On unix a reaped entry takes a third leftover of the same death with
it: the private `pkc-…` directory and socket that record published as its control
endpoint, which nothing else ever cleaned up — reaped only after the endpoint passes a
strict shape check (it is untrusted data) and without ever following a symlink.
Without `--json` it prints a one-line summary (`no stale entries to prune` when there
was nothing to do); with `--json` it prints a single JSON object
`{"pruned":N,"live":N,"unprobed":N,"orphaned_locks":N}` — unchanged, since a reaped
socket is counted by its own entry. An empty or never-created registry is not an error
— `prune` reports a zero tally and exits `0`, and pruning a missing registry does not
create it. See [`docs/registry.md`](docs/registry.md), "Reaping — `prune`".

`prune --dry-run` previews that same reap without deleting anything: the identical
scan and the identical liveness classification, just never a `fs::remove_file`, so an
operator can see what a real `prune` would do before committing to it. Its aggregate
counts are exactly what a following real `prune` over the same, untouched registry
state would report. Without `--json` it lists each confirmed-stale candidate (a paired
entry's `run_id`/`started_at`, plus ` socket_dir=<path>` when a control socket would
be reaped with it, or an orphaned lock's file name) followed by a "would prune …"
summary line; with `--json` (`prune --dry-run --json`) it prints the same
`pruned`/`live`/`unprobed`/`orphaned_locks` fields plus an additional `candidates`
array of the same candidates, each tagged `"kind":"entry"` (with
`run_id`/`started_at`/`socket_dir`) or `"kind":"orphaned_lock"`. See
[`docs/registry.md`](docs/registry.md), "Reaping — `prune`".

## Detached runs

`--detach` hands the run to a **detached copy of this binary** and returns as soon
as that copy has provably started it, instead of keeping the caller as the runner's
parent for the whole run. The copy is re-spawned on the caller's own command line
(minus `--detach`): on Unix in a new session (`setsid`), so a terminal hang-up or a
`Ctrl-C` in the caller's session no longer reaches it; on Windows with
`DETACHED_PROCESS`, so it holds no console handle. Everything else about the run is
unchanged — same container, same registry entry, same control transport, same JSONL
stream, same teardown — because the detached copy runs the very same code path a
foreground `run` does.

```sh
processkit-cli run --detach --run-id build-42 --jsonl build-42.jsonl -- cargo build
# returns as soon as the run has started; supervise it from anywhere:
processkit-cli inspect --run-id build-42 --json
processkit-cli wait --run-id build-42
```

- **"Started" is an observation, not an assumption.** The call returns only once the
  detached runner's `run_started` event is readable in `--jsonl`, which it writes
  *after* creating the container, publishing the registry record, and spawning the
  child — so on return the run is already discoverable by `list`, reachable by
  `inspect`/`cancel`/`kill`, and waitable by `wait`. That also means the run id is
  always readable from the events file on return, so a caller that let the runner
  generate one can still find it. The call's own streams end with it, too: a caller
  that *captures* output (a shell pipeline, `subprocess.run(capture_output=True)`)
  sees end-of-file when the call returns, not when the run ends — the detached runner
  is left holding none of the caller's handles.
- **The exit code reports the start, and only the start.** This is the one mode in
  which the runner's exit code is *not* the child's: a confirmed start is `0` even
  for a child that later fails, and a start that failed keeps the same reserved-band
  code the run would have reported in the foreground — a missing program is still
  `SPAWN` (`101`), an unusable container still `BACKEND` (`102`), an unwritable
  `--jsonl` still `SETUP` (`111`). A failed start is never reported as success. The
  child's real exit code is not lost, it simply moves to where a detached caller can
  observe it: the terminal `runner_exit` event in `--jsonl` (see
  [the exit-code contract](docs/exit-codes.md), "Detached runs").
- **No live echo.** The detached runner runs with `--no-echo`'s discarding sinks —
  there is nobody left to echo to — while `--capture-dir`, `--idle-timeout`, and the
  JSONL stream keep observing the child's output exactly as in the foreground.
  `--jsonl` stays required, and is the only channel a detached caller has left.
- **Not for interactive runs.** `--detach` conflicts with `--inherit-stdio` and
  `--inherit-stdin` at parse time: a detached run has no terminal to hand over. The
  detached copy's own stdin/stdout/stderr are `null`, so it can neither read from nor
  write into a caller's terminal or pipe after that caller is gone.
- **Windows:** because the detached runner has no console of its own, a console child
  gets a fresh one from the OS — a stray console window unless `--create-no-window`
  is passed as well, which is exactly what that flag is for (see
  [Windows console](#windows-console)). A detached process also stays in any Job
  Object the caller belongs to, so a caller inside a kill-on-close job cannot detach
  a run out of it.
- **Unix:** the detached copy is this process's direct child only until this process
  exits moments later; `init` adopts it from there. If the start never completes
  within its startup budget, the detached copy is killed rather than left behind
  unreported — with the same reach an abrupt runner death has on this platform (the
  `abrupt_cleanup` tri-state above).

## Exit codes

The runner's exit code **is** the child's exit code; the runner's own failures
(bad arguments, spawn failure, backend error) and the four runner-*imposed*
endings — a `--timeout` (`106`), a local stop-signal cancel (`107`: `Ctrl-C`, on
Unix `SIGTERM`/`SIGHUP`, or on Windows `Ctrl-Break`/console close/logoff/system
shutdown), a control-plane `cancel`
(`108`), and a control-plane `kill` (`109`) — use a distinct, reserved code band so
they can never be mistaken for a child result, and so each ending is tellable from the
others by code alone. This is part of the project's compatibility surface — see
[the exit-code contract](docs/exit-codes.md). The preflight `probe` adds one code
to the reserved band — `PROBE_INCOMPATIBLE` (`110`) — for an incompatible launch
candidate, and `wait` adds one more — `WAIT_TIMEOUT` (`112`) — for its own deadline
elapsing while the run it was watching is still live. That last one describes the
*waiting client*, not the run: nothing was stopped, so it is deliberately distinct
from the run's own `TIMEOUT` (`106`).

`run --detach` is the single exception to the first sentence, and it mints no code
of its own: with the run handed to a detached copy there is no child left for this
process to forward a code from, so the code reports whether the run *started* — `0`
once it has, otherwise the very same reserved code the failed start would have
reported in the foreground. See [Detached runs](#detached-runs) above.

## Timeouts, cancel, and grace

In the default and input-only I/O modes, `run` bounds a run three ways — a
whole-run deadline, an idle (no-output) deadline, and a cancel signal (interactive
or external) — and all of them end in the **same** teardown path:

- `--timeout <duration>` is a hard deadline for the whole run. When it elapses the
  runner ends the run and exits with the reserved `TIMEOUT` code (`106`) — never
  the child's own code, because the child did not choose to stop.
- `--idle-timeout <duration>` is a deadline on child **silence**, for the classic
  stuck-worker case (a build worker that is alive but has long stopped making
  progress). Its deadline is *re-armed* on every chunk of the child's output, so a
  child that keeps talking is never reaped no matter how long it runs — only one
  that goes quiet past the window is. An idle expiry reuses the **same** `TIMEOUT`
  code (`106`) and the same teardown as `--timeout`; the two are told apart by the
  `timeout` event's `reason` field (`overall` vs `idle`) in the JSONL stream, not by
  a distinct exit code. Because it needs the runner's output pump to observe the
  child, it cannot be combined with `--inherit-stdio` (a parse-time error, like
  `--capture-dir`); it does compose with `--capture-dir`, which re-arms the same one
  timer through its tee.
- **`Ctrl-C`** cancels a run in progress. The runner ends it and exits with the
  reserved `CANCELLED` code (`107`), distinct from a timeout and from any child
  code, so "I interrupted it" is never confused with "it ran too long" or with a
  child that merely returned non-zero.
- **`SIGTERM` and `SIGHUP` (Unix)** are caught too, and take the *same* path: an
  external `kill <pid>`, a `systemctl stop`, a cancelled CI job, or a hung-up
  terminal ends the run through the full teardown rather than killing the runner
  where it stands. This matters because the default disposition of those signals
  would skip teardown entirely — no terminal JSONL events, a registry entry left
  behind, and no explicit kill of the container, whose abrupt-owner-death reap
  covers only the direct child on Linux and nothing on macOS (the `abrupt_cleanup`
  tri-state under [Platform matrix](#platform-matrix)). A signal your environment
  has deliberately neutralized before launching the runner (`SIG_IGN` — which is
  exactly what `nohup` does to `SIGHUP`) is **left alone**: the runner does not
  un-ignore it behind your back, and nothing is lost by that, since an ignored signal
  would not have stopped the runner either.
- **`Ctrl-Break`, console close, logoff, and system shutdown (Windows)** are caught
  too, through the same console-control-handler mechanism `Ctrl-C` already used, and
  take the *same* path. Uncaught, their default handling would likewise skip
  teardown entirely: the terminal JSONL events (`cancelled`, `cleanup_started`,
  `cleanup_finished`, `runner_exit`) would never be written and the run's registry
  entry would be left behind stale until `prune` — the run would go dark to any
  observer of the event stream or registry, even though the tree itself is not left
  orphaned: closing the runner's last Job Object handle still reaps it (`whole_tree`
  on Windows, the `abrupt_cleanup` tri-state under [Platform matrix](#platform-matrix)).
  Catching these events turns that invisible-but-contained ending into a reported,
  ordinary one. The console-close event carries an OS-imposed deadline: Windows
  gives the handler only about 5 seconds before terminating the process regardless,
  so the runner caps the *effective* `--grace`
  for that one trigger to a budget comfortably under that window — a longer request
  degrades to the shorter, honest wait rather than risking the OS killing the runner
  before it even finishes reporting the teardown. Logoff and shutdown are left
  uncapped, since their real deadline is a system-wide policy this runner cannot
  reliably discover (see the `wait_for_cancel_signal`/`effective_grace_for` doc
  comments in `src/run/signals.rs` for the full reasoning). **Known limitation:** unlike a
  repeat Unix `SIGTERM`/`SIGHUP`/`Ctrl-C` mid-teardown (silently absorbed — the OS
  keeps the disposition installed for the process's lifetime regardless of listener
  state), a *second* Windows console-control event that arrives after teardown has
  already begun is **not** absorbed: it falls through to the OS's default handling
  and terminates the runner outright, mid-teardown, before the terminal JSONL events
  are written. This is an accepted trade-off, not a bug — see the `#[cfg(windows)]`
  arm of `wait_for_cancel_signal` in `src/run/signals.rs` for why keeping listeners
  alive for the whole teardown was rejected.

All of the signals/events above share the `CANCELLED` code (`107`); which one
arrived is recorded in the JSONL `cancelled` event's `source` field (`ctrl_c` /
`sigterm` / `sighup` (Unix) / `ctrl_break` / `ctrl_close` / `ctrl_logoff` /
`ctrl_shutdown` (Windows)).

With `--inherit-stdio`, native terminal delivery takes precedence; see
[Standard I/O](#standard-io) for the platform-dependent reporting contract.

`--grace <duration>` sets the pause between the *soft* stop and the *hard* kill on
both paths: the runner first asks the process tree to stop, waits up to the grace
window, and only then hard-kills whatever remains. The hard kill is always the
owning container's kernel-backed **kill-on-drop** (a Windows Job Object close, a
Linux cgroup / POSIX-group teardown), so the whole tree — including any leaked
grandchild — is reaped on every ending.

Durations use a small grammar: a non-negative integer with an optional unit —
`ms`, `s` (the default), `m`, or `h` (e.g. `30`, `500ms`, `5s`, `2m`, `1h`). A
malformed value is a usage error (exit `100`), not a surprise at runtime. `0` is
legal for `--grace` — it means "no pause" between the soft stop and the hard
kill. It is **not** legal for `--timeout`/`--idle-timeout`: a `0` there would arm
a deadline that is already elapsed on the very first poll, tearing the child down
immediately after spawn — almost certainly an operator typo rather than an
intentional immediate teardown — so both flags reject a total of `0` (in any
unit) at parse time, the same "degenerate cap" treatment `--max-memory`,
`--max-processes`, and `--cpu-quota` already give their own `0`. Omit either flag
to leave that deadline unbounded instead.

**Honest degradation on Windows.** A Windows Job Object has no POSIX signal, so the
soft stop there is not a signal at all: ProcessKit makes a best-effort soft *close*
of whatever the tree exposes — a `WM_CLOSE` to every top-level window owned by a live
member. A plain console child owns no window, so for the ordinary Windows run
**nothing** is delivered: the grace window still elapses, and then the Job Object is
killed atomically. The runner never pretends a graceful soft-terminate happened when
it could not — the stderr line for such a timeout/cancel says plainly that nothing in
the tree could receive a soft close and that it was hard-killed via the Job Object
after the grace delay. When the tree *does* contain a windowed member, the close is
delivered and reported as `signalled` (see the `cleanup_finished` event's
`soft_terminate` field in
[the JSONL event schema](#jsonl-event-schema)) — never as a signal, which Windows
cannot send. On Unix the soft stop is a real `SIGTERM` to the tree.

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
pass `--create-no-window` explicitly. It cannot be combined with
`--inherit-stdio`: suppressing the child's console and promising to preserve the
caller's terminal are contradictory requests.

A [detached run](#detached-runs) is the case where passing it matters most: the
detached runner is created without a console at all, so a console child cannot
inherit one and Windows gives it a fresh console of its own — visible unless
`--create-no-window` says otherwise.

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
It cannot be combined with `--inherit-stdio`, because direct child writes never
pass through the runner's bounded tee.

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

`--capture-dir` is independent of `--no-echo`: capture is fed by the same tee
regardless of whether the live echo is suppressed, so a run with both flags
records exactly the same `stdout.log`/`stderr.log` bytes and `output_captured`
event as one with `--capture-dir` alone — only the runner's own stdout/stderr
retransmission is affected by `--no-echo` (see [Standard I/O](#standard-io)).

The per-stream ceiling defaults to 8 MiB and is configurable with
`--capture-max-bytes <size>`, only meaningful together with `--capture-dir`.
Same grammar as `--max-memory`: a byte count with an optional binary unit —
`1048576`, `512k`, `256m`, `2g` (`k`/`m`/`g` are KiB/MiB/GiB, 1024-based). A
malformed value is a usage error (exit `100`) rejected at parse time, not a
silent fallback to the default. Omitting the flag leaves the default 8 MiB
ceiling in place, so a bare `run --capture-dir` (no `--capture-max-bytes`)
stays byte-for-byte unchanged; `output_captured`'s shape and its `truncated`
flag's meaning are unaffected — `truncated` still just means "the stream
outran whichever per-stream ceiling was in effect," regardless of its size.
`--capture-max-bytes` is independent of the pump's separate in-flight
line-assembly ceiling (64 MiB, not user-configurable): that one bounds a
single still-unterminated line's in-memory assembly, a pathological-input
concern sized once for the whole binary, while `--capture-max-bytes` bounds
the on-disk per-stream transcript size, an operator's disk-budget choice per
invocation — the two are deliberately kept separate rather than one deriving
from the other.

## Resource limits

Three optional flags cap the run's **whole process tree** — not a single process —
mapping straight onto ProcessKit's own `ProcessGroupOptions` resource caps (the
runner never reimplements the OS-level enforcement):

- `--max-memory <size>` — total tree memory. A byte count with an optional binary
  unit: `1048576`, `512k`, `256m`, `2g` (`k`/`m`/`g` are KiB/MiB/GiB, 1024-based).
- `--max-processes <n>` — number of live processes in the tree (`n` > 0).
- `--cpu-quota <cores>` — CPU as a fraction of a **single** core: `0.5` is half a
  core, `2` is two cores' worth (a finite value > 0).

Omit a flag to leave that resource unbounded; a bare `run` requests no caps at all
and behaves exactly as before. A nonsensical value (`--max-memory 0`, a
non-positive or non-finite `--cpu-quota`, `--max-processes 0`) is a usage error
(exit `100`) rejected at parse time, not a surprise at runtime.

**Enforcement is platform-specific, and honestly fail-fast — never a silent
no-op.** A whole-tree cap needs a real container:

| Platform / mechanism | Resource limits |
| --- | --- |
| Windows — Job Object | **Enforced** (memory, CPU, and active-process count). |
| Linux — cgroup v2 **at the real hierarchy root** | **Enforced.** Requires the runner to be a direct member of the real cgroup-v2 root (a minimal, non-systemd init). **Not** under a systemd session/scope/service, and **not** in an ordinary container (incl. Docker/Kubernetes and **typical CI such as GitHub Actions**) — there the controllers can't be enabled and the request is *unenforceable*. |
| macOS, the BSDs, Linux process-group fallback | **Unsupported** — no whole-tree container exists, so any cap request fails fast. |

Where a cap **cannot** be applied, the run does not silently continue unbounded:
it fails fast **before the child is spawned**, emits a `limit_hit` JSONL event
naming which limit (`memory` / `processes` / `cpu`), and exits with `BACKEND`
(`102`) — the `limit_hit` event, not the exit code, is the authoritative signal
that the ending was a resource limit (see [the exit-code
contract](docs/exit-codes.md)). An adapter that *depends* on a cap should treat a
`limit_hit` as a hard failure rather than a warning.

**Linux `--max-processes` caveat.** On Linux the kernel checks the process-count
cap only when a process forks *inside* the cgroup, so `--max-processes` reliably
bounds the **descendants** a contained child spawns (its own fork bomb), but does
**not** reject additional top-level launches into the group. Treat it as a bound on
a tree's own growth, not an exact admission count. (Memory and CPU caps are
whole-cgroup and have no such asymmetry; on Windows the Job Object's
active-process limit does cap every launch into the job.)

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
launch and container errors (including a `limit_hit` when a `--max-memory` /
`--max-processes` / `--cpu-quota` cap could not be applied — see "Resource
limits"), an `output_captured` event when `--capture-dir` is set (see "Bounded
output capture"), and a terminal `runner_exit` that preserves the child's own code
even when the runner itself fails — so a child's code is never lost or aliased.

The command line is **redacted by default** (`argv` is recorded only under
`--argv-raw`); in its place a one-way SHA-256 fingerprint of argv (`argv_sha256`)
and, for recognized worker shapes, a categorical `hint` are recorded on every run —
neither can reveal the command line. Member snapshots carry each member's `ppid`,
executable `name`, and start-time token wherever ProcessKit's `members_info()` can
report them (nullable per-field on platforms/members it can't).

- Normative field reference: [`docs/schema.md`](docs/schema.md).
- Golden sample stream for adapters:
  [`fixtures/schema/v1/events.jsonl`](fixtures/schema/v1/events.jsonl).
- Machine-readable JSON Schema (draft 2020-12) for the same contract:
  [`fixtures/schema/v1/schema.json`](fixtures/schema/v1/schema.json).
- **Getting the schema without a git checkout.** A consumer holding only an
  installed binary or an unpacked release archive — no clone, no tag to match —
  has two offline ways to get the exact schema its own version emits: run
  `processkit-cli probe --json --print-schema`, which prints the schema document
  embedded in that binary at build time (byte-for-byte identical to the
  `schema.json` above), or unpack the release archive and read the bundled
  `schema/schema.json` and `schema/events.jsonl` it ships alongside the binary
  and the shell completions/man pages.

## Status

`run` is implemented: it spawns the child into a ProcessKit container the runner
owns, echoes the child's output live by default or directly inherits all three
standard handles with `--inherit-stdio`, forwards the child's exit code exactly,
enforces `--timeout`, `--grace`, and stop-signal cancellation (`Ctrl-C`, plus
`SIGTERM`/`SIGHUP` on Unix, plus `Ctrl-Break`/console close/logoff/system shutdown
on Windows) with a guaranteed
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
entry, whatever its health (`live`/`stale`/`unprobed`), as a table or (with
`--json`) as JSON Lines — the discovery counterpart for a caller that has lost or
never had a `run_id` — and `prune` reaps the confirmed-stale leftovers it shows. `wait`
blocks on that same registry until its target — one named run (`--run-id`) or every
run confirmed live in a snapshot taken at the start (`--all`) — is no longer live, for
a supervisor that is not the runner's parent, bounding itself with `--timeout` and its
own reserved exit code (`112`). `probe` reports and
verifies the binary's compatibility surface for a consumer's fail-closed launcher
preflight, with no side effects, exiting `PROBE_INCOMPATIBLE` (110) on an
incompatible candidate; `probe --print-schema` instead prints the binary's
embedded JSONL event-schema document and exits (see "JSONL event schema"). See
[the roadmap](docs/ROADMAP.md) for delivery status and the remaining
ProcessKit-rs dependencies.

## Development

```powershell
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
cargo run --bin processkit-cli -- --help
```

## Benchmarks

A [criterion]-based regression lane lives under `benches/`, gated behind the
`bench` Cargo feature so a plain `cargo build`/`cargo test`/`cargo publish`
never builds it — like the `e2e` end-to-end tier, this is dev-only tooling. It
covers two things:

- **Internal primitives**, in isolation: incremental SHA-256
  (`src/hash.rs`, `benches/hash_bench.rs`), bounded-capture `absorb`
  (`src/capture.rs`, `benches/capture_bench.rs`), and the argv worker-shape
  hint classifier (`src/events.rs`, `benches/hint_classifier_bench.rs`). Each
  reaches the primitive directly rather than through its caller (see each
  module's doc comment on why a given item is `pub`) — the library is not a
  stable API (see "Target structure: library and binary" in
  [`docs/architecture.md`](docs/architecture.md)), and reaching internals
  directly for exactly this purpose is part of its documented design.
- **Through-the-binary scenarios**, driving the built binary like the
  integration tests do: `benches/echo_overhead_bench.rs` compares a direct
  invocation of a child that writes an exact byte count against the same child
  run under `run` (plain echo, and echo plus `--capture-dir`) at a couple of
  payload sizes; `benches/startup_latency_bench.rs` measures the latency from
  just before `run` is invoked to the `time` the runner itself stamps on its
  `run_started` JSONL event, over a short series of invocations.

Run the whole lane locally with:

```sh
cargo bench --features bench
```

CI publishes results to the `perf` job's step summary on every push/PR — a
data source, not a gate: the job never fails the pipeline and is not a
required check (same non-gating pattern as the `coverage` job; see
`CONTRIBUTING.md`, "Code coverage"). Absolute numbers are noisy on shared CI
hardware; treat the summary as a trend over time, not a single-run verdict.

The table below is one informal, single-run snapshot from a Windows
development host (not tuned, not a target) — a sanity baseline for a reader
skimming this document, not the authoritative source; run the lane yourself
for numbers on your own hardware, or read the CI step summary for the
tracked trend.

| Benchmark | Result |
| --- | --- |
| Incremental SHA-256, 64KiB chunks (256KiB total) | ~116 MiB/s |
| `StreamCapture::absorb`, 64KiB chunks (256KiB total) | ~98 MiB/s |
| Hint classifier, no match / MSBuild match | ~520 ns / ~830 ns |
| Echo overhead, 4MiB payload: direct vs. under `run` (no capture) | ~16 ms vs. ~3.8 s |
| Echo overhead, 4MiB payload: under `run` with `--capture-dir` | ~3.3 s |
| Startup latency, call to `run_started` | ~430 ms (median) |

The echo-overhead and startup-latency figures are dominated by this run's own
per-invocation costs (container creation, the registry record, the control-
plane endpoint, and the async pump/echo path) rather than by the child's own
payload size — a multi-hundred-millisecond-to-low-seconds cost per `run`
invocation on this host, worth keeping an eye on as the regression lane
accumulates history, but not (yet) a claim about where the time goes.

[criterion]: https://github.com/bheisler/criterion.rs

## Safety boundaries

- No reimplementation of ProcessKit containment or lifecycle behavior.
- No shell mode, PTY support, or global cleanup by executable/process name.
- Raw command arguments are opt-in only; the default diagnostics contract uses
  redaction-safe hashes and worker hints.
