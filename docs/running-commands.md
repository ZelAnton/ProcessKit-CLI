# Running commands

`run` executes exactly one program inside a ProcessKit container and keeps the
runner alive until the child and its descendants have been reaped. The program
is always passed after `--`:

```text
processkit-cli run [RUNNER OPTIONS] --jsonl <events.jsonl> -- <program> [args...]
```

The separator makes the ownership boundary explicit. Everything before `--`
belongs to `processkit-cli`; everything after it is the child argv.

## Shell-free argv

There is no shell mode. The runner does not expand wildcards, interpolate
variables, interpret pipes, or process redirection characters:

```sh
processkit-cli run --jsonl run.jsonl -- git status --short
```

This starts `git` directly with two arguments. A string such as `*.rs`, `$HOME`,
`%TEMP%`, `|`, or `>` remains a literal argument unless the child itself gives
it special meaning.

If a shell is genuinely required, name it as the program so that the security
and quoting boundary is visible at the call site:

```sh
processkit-cli run --jsonl shell.jsonl -- sh -c 'printf "%s\n" "$HOME"'
```

```powershell
processkit-cli run --jsonl shell.jsonl -- pwsh -NoProfile -Command 'Get-Date'
```

Prefer direct execution whenever possible. It preserves argv boundaries and
avoids a second language's quoting and injection rules.

## Working directory

By default the child inherits the runner's current directory. Set it explicitly
when the caller and payload use different roots:

```sh
processkit-cli run --cwd ./service --jsonl service.jsonl -- cargo test
```

The runner resolves the effective directory before launch and records its
absolute form in `run_started.cwd`. An invalid or inaccessible directory fails
before the child starts.

## Environment

Without environment flags, the child inherits the runner's environment. Four
options make the result explicit:

| Flag | Effect |
| --- | --- |
| `--env-clear` | Start from an empty environment. |
| `--env-remove KEY` | Remove one inherited key. |
| `--env-file FILE` | Read UTF-8 `KEY=VALUE` lines without placing their values in argv; blank lines and `#` comments are ignored. |
| `--env KEY=VALUE` | Set or replace one key. The value may contain `=`. |

Application order is fixed, regardless of flag order: **clear, remove, env-file,
explicit set**. Repeated files are applied in argument order, and an explicit
`--env` therefore wins over every file or removal for the same key. A file read,
UTF-8, or syntax failure is `SETUP` (111) before the child starts.
For either `--env` or a file entry, the key must be non-empty and contain no
whitespace or control characters. Diagnostics for malformed entries do not
repeat their values.

```sh
processkit-cli run \
  --env-clear \
  --env PATH=/usr/bin:/bin \
  --env CI=true \
  --jsonl env.jsonl \
  -- /usr/bin/env
```

Environment values are not copied into lifecycle events or registry records.
They can still reach child output, so capture files and echoed output require
the same secret-handling discipline as any other process log.

## Run identity

`--run-id` gives the run a stable application-level name:

```sh
processkit-cli run \
  --run-id build-2026-07-27 \
  --jsonl build.jsonl \
  -- cargo build
```

When omitted, the runner generates an id and writes it in `run_started`. A
caller that needs to address the live run immediately should provide an id or
read the first event before calling `inspect`, `cancel`, `kill`, or `wait`.
Explicit ids must contain 1-256 Unicode characters and cannot contain terminal
control or invisible formatting characters. The same validation applies to
every by-id command, so an unsafe id is rejected before registry access.

Run ids are not operating-system PIDs and are not required to be globally
unique. Two live runs with the same explicit id make by-id control ambiguous;
the client fails rather than choosing one. `list` shows every record, and the
aggregate `--all` commands address records rather than resolving a shared id.

Repeat `--label KEY=VALUE` on `run` to attach non-secret operator metadata. The
resulting map appears in `run_started.labels` and `list`; later values replace
earlier values for a duplicate key. Keys are 1-64 ASCII bytes, begin with a letter
or `_`, and otherwise contain letters, digits, `.`, `-`, or `_`. Values are at most
256 characters and, like an explicit run id above, cannot contain terminal control
or invisible formatting characters; the same check runs on read, so a label an
older record already carries is dropped from that record's map when it fails, while
the record itself is kept. Repeated label filters on `cancel --all`, `kill --all`,
and `wait --all` combine with logical AND.

## Foreground lifecycle

A foreground invocation has four high-level phases:

1. Validate arguments and open the JSONL destination.
2. Create a ProcessKit container, spawn the child, and publish the registry
   record/control endpoint.
3. Wait for child exit, timeout, signal, or control-plane action while pumping
   output when the selected I/O mode requires it.
4. Tear down the owned container, write terminal events, remove the registry
   record, and exit.

On normal completion the runner exits with the child's exact code. A runner
failure or runner-imposed ending uses the reserved band documented in
[Exit-code contract](exit-codes.md).

## Child stdout and stderr

The default mode pipes both child streams through ProcessKit and echoes them to
the runner's matching streams. JSONL never goes to stdout. This allows a parent
to treat `processkit-cli` like the command it wraps while independently tailing
the lifecycle file.

For terminal inheritance, stdin sources, echo suppression, and bounded
transcripts, see [Standard I/O and capture](io-and-capture.md).

## Time bounds and cancellation

```sh
processkit-cli run \
  --timeout 20m \
  --idle-timeout 2m \
  --grace 5s \
  --jsonl bounded.jsonl \
  -- cargo build
```

The overall and idle clocks are independent. Either expiry enters the shared
soft-stop → grace → hard-kill path. `Ctrl-C` and `cancel` use the same teardown
shape but remain distinguishable outcomes. See
[Timeouts and cancellation](timeouts-and-cancellation.md).

## Recorded tree snapshots

```sh
processkit-cli run \
  --snapshot-interval 30s \
  --jsonl long-run.jsonl \
  -- ./build-fleet
```

By default the tree's shape is recorded once, right after spawn.
`--snapshot-interval` re-emits that same `members_snapshot` event on the given
cadence while the child runs, so a long, quiet, or detached run leaves a recorded
history of how the tree evolved rather than one that is only observable *live*
via `inspect`. The periodic events are told apart from the post-spawn one by the
event's `reason` field (`interval` vs `spawn`), and stop as soon as the run's
ending is decided. See [JSONL event schema](schema.md#members_snapshot).

## Resource limits

```sh
processkit-cli run \
  --max-memory 2g \
  --max-processes 64 \
  --cpu-quota 2 \
  --jsonl limited.jsonl \
  -- build-worker
```

Limits apply to the whole contained tree where the active mechanism can
enforce them. Unsupported limits fail before launch instead of silently
running unbounded. Platform constraints are collected in
[Resource limits](resource-limits.md).

## Windows console policy

`--create-no-window` maps to Windows `CREATE_NO_WINDOW` and is a no-op on other
platforms. It is intentionally opt-in: a normal run should not suppress a
console the child legitimately expects.

It conflicts with `--inherit-stdio`, because hiding the child's console and
promising to preserve the caller's terminal are contradictory. It is especially
useful with [detached runs](detached-runs.md), whose detached runner owns no
console for a console child to inherit.

`--windows-graceful-ctrl-break` opts a Windows console child into ProcessKit's
cooperative `CTRL_BREAK` tier before Job Object escalation. It is a no-op off
Windows and conflicts with both consoleless modes (`--create-no-window`, `--detach`).

## Flag interaction summary

| Combination | Result |
| --- | --- |
| `--capture-dir` + `--no-echo` | Valid: capture continues; only live echo is suppressed. |
| `--idle-timeout` + `--capture-dir` | Valid: the shared pump drives both. |
| `--inherit-stdio` + `--capture-dir` | Rejected: inherited output bypasses the pump. |
| `--inherit-stdio` + `--idle-timeout` | Rejected: the runner cannot observe activity. |
| `--inherit-stdio` + `--snapshot-interval` | Valid: snapshots read the container, not the pump. |
| `--detach` + `--inherit-stdio` | Rejected: the caller is no longer present. |
| `--detach` + `--capture-dir` | Valid: the detached runner still captures. |
| `--create-no-window` + `--inherit-stdio` | Rejected on every platform at parse time. |
| `--windows-graceful-ctrl-break` + `--create-no-window`/`--detach` | Rejected: `CTRL_BREAK` needs a shared console. |

Parse-time conflicts are usage failures (`100`); no child is spawned.

## Next steps

- [Cookbook](cookbook.md) for complete command shapes.
- [Detached runs](detached-runs.md) for out-of-band supervision.
- [JSONL event schema](schema.md) for the machine-readable lifecycle.
- [Live-run control plane](control-plane.md) for `inspect`, `cancel`, and `kill`.
