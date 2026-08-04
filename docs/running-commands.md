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

Without environment flags, the child inherits the runner's environment. Five
options make the result explicit:

| Flag | Effect |
| --- | --- |
| `--env-clear` | Start from an empty environment. |
| `--env-remove KEY` | Remove one inherited key. |
| `--env-file FILE` | Read UTF-8 `KEY=VALUE` lines without placing their values in argv; blank lines and `#` comments are ignored. |
| `--env KEY=VALUE` | Set or replace one key. The value may contain `=`. |
| `--run-id-env KEY` | Set one key to this run's final id (see [Publishing the run id to the child](#publishing-the-run-id-to-the-child)). |

Application order is fixed, regardless of flag order: **clear, remove, env-file,
explicit set, run-id injection**. Repeated files are applied in argument order, an
explicit `--env` therefore wins over every file or removal for the same key, and
`--run-id-env` — applied last — wins over all of them. A file read,
UTF-8, or syntax failure is `SETUP` (111) before the child starts.
Every flag that names a key holds it to one rule: non-empty, and no whitespace or
control characters. Diagnostics for malformed entries do not
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

### Publishing the run id to the child

`--run-id-env KEY` sets `KEY` in the child's environment to this run's **final**
id — the explicit `--run-id` when one was given, otherwise the id the runner
generated:

```sh
processkit-cli run \
  --run-id-env PROCESSKIT_RUN_ID \
  --jsonl build.jsonl \
  -- ./build.sh
```

The injected value is the same one `run_started.run_id`, the registry record,
and every control-plane reply carry, so the child (and anything it spawns) can
correlate its own work with the run without the caller minting an id itself and
passing it twice as `--run-id <id> --env KEY=<id>` — which duplicates an identity
that can then drift, and rules out a generated id, since a generated id is not
knowable outside the run until the run has started.

The flag is strictly opt-in: omit it and nothing is injected. No key is set by
default.

Collisions are settled without reference to flag order:

| Also given | Result |
| --- | --- |
| `--env-clear` | The injection is applied after the clear, so the key survives. |
| `--env-remove KEY` | The injection is applied after removals, so the key is set. |
| `--env-file` entry for `KEY` | The injection is applied last, so the run id wins. |
| `--env KEY=VALUE` | **Refused** at parse time as `USAGE` (100), before anything runs. |

The last row is the deliberate choice: an explicit `--env` for the same key asks
for a different value of one variable, and silently discarding what the caller
typed would be worse than refusing the pair. The refusal names the key and never
repeats the value beside it.

"The same key" is decided the way the platform decides it. Windows environment
names are case-insensitive, so `--env KEY=value --run-id-env key` names one child
variable there and is refused just like the identical spelling; on other
platforms `KEY` and `key` are two variables, so that pair is accepted and each
keeps its own value.

The value is **correlation data, not a credential or a security proof**. It says
which run some work belongs to; it does not establish who started that run,
carries no authority, and can be set by anything able to write an environment
variable. Do not use it to authorize anything — see
[Threat model](threat-model.md).

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
Whichever way the id was decided, `--run-id-env KEY` publishes that final value
into the child's environment (see [Publishing the run id to the
child](#publishing-the-run-id-to-the-child)).
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
ending is decided. A sample whose member read fails is still recorded, carrying
`read_error: true` and an empty member list rather than being dropped. See
[JSONL event schema](schema.md#members_snapshot).

It composes with every I/O mode, including `--inherit-stdio`, and it is forwarded
by `--detach` — see [Detached runs](detached-runs.md), the scenario it exists for.

### Sizing the stream: there is intentionally no ceiling

Before this flag, a run's JSONL file held a fixed handful of events regardless of
how long the run took. A cadence changes that: the file now grows with the run's
*duration*, and the runner imposes **no** ceiling, rotation, or sampling on it.
That is a deliberate choice, not an oversight, and it is the opposite of the choice
made for captured child output (`--capture-max-bytes` with an explicit
`--capture-overflow truncate|cancel` policy). Three reasons:

- **The operator already holds the dial.** Output volume is dictated by the child
  and is unknowable in advance, which is why capture needs a runtime ceiling. Here
  the volume is a direct, computable consequence of a number the operator typed —
  a ceiling would only second-guess an explicit instruction.
- **Neither truncation policy fits.** Silently dropping later snapshots would
  destroy exactly the end-of-run history the feature exists to record, and ending
  the run over a diagnostics budget would let an observability option kill the
  payload it was only meant to watch.
- **The bound belongs where the file does.** The stream's destination is the
  operator's `--jsonl` path; disk budgets, retention, and rotation are properties
  of that location, not of the runner.

**The arithmetic.** One snapshot line costs roughly **130 bytes** of envelope plus
about **80 bytes per member** of the tree, and the run writes
`duration / interval` of them:

| Run | Cadence | Tree | Lines | Added to `--jsonl` |
| --- | --- | --- | --- | --- |
| 1 hour | `30s` | 10 | 120 | ≈ 110 KB |
| 8 hours | `1m` | 100 | 480 | ≈ 4 MB |
| 24 hours | `1m` | 100 | 1 440 | ≈ 12 MB |
| 24 hours | `1s` | 100 | 86 400 | ≈ 700 MB |

**Choosing an interval.** Pick the coarsest cadence that still answers the question
you expect to ask; the resolution that matters is usually "when did this change",
not "what was it at every instant". Seconds-scale intervals are for short
investigations, minutes-scale for day-long or detached runs. As a rule of thumb keep
the projected line count in the thousands rather than the millions — with a
100-process tree that is a few tens of megabytes, which any run that was worth
supervising can afford.

**If the file cannot be written**, the runner reports the failure once on stderr and
then stops emitting events for the rest of the run; the run itself continues. A full
disk therefore shows up as a stream that simply ends mid-run, and in a detached run
(whose stderr is `null`) with no warning anywhere. This is the pre-existing behavior
of the JSONL emitter, not something the cadence introduces, but a long cadence is the
most likely way to meet it — one more reason to size the interval rather than rely on
a ceiling that does not exist.

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
