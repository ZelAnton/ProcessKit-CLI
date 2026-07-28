# Timeouts and cancellation

ProcessKit CLI has separate outcomes for a child exit, overall timeout, idle
timeout, local cancellation, control-plane cancellation, and immediate kill.
They share teardown machinery without collapsing their meaning.

## Overall deadline

```sh
processkit-cli run --timeout 10m --jsonl run.jsonl -- cargo test
```

`--timeout` bounds wall-clock lifetime from launch. When it expires, the runner
enters soft-stop → grace → hard-kill teardown and exits `TIMEOUT` (`106`). The
child did not choose that code; the terminal `runner_exit.child_code` is
therefore separate.

## Idle deadline

```sh
processkit-cli run \
  --idle-timeout 90s \
  --capture-dir ./capture \
  --jsonl run.jsonl \
  -- build-worker
```

`--idle-timeout` measures silence, not total duration. Every observed stdout or
stderr chunk re-arms the clock. A chatty ten-hour process can remain healthy,
while a stuck worker that produces no output for 90 seconds is torn down.

Idle detection requires the output pump. It conflicts with
`--inherit-stdio`, but composes with capture and `--no-echo`. Overall and idle
deadlines may be used together; the first to expire wins.

Both expiries use code `106`. The JSONL `timeout.reason` field distinguishes
`overall` from `idle`.

## Duration grammar

Durations are non-negative integers with an optional unit:

| Input | Meaning |
| --- | --- |
| `30` | 30 seconds |
| `500ms` | 500 milliseconds |
| `5s` | 5 seconds |
| `2m` | 2 minutes |
| `1h` | 1 hour |

`0` is rejected for overall, idle, and wait deadlines because it expires before
the first meaningful check. `--grace 0` is valid and requests no pause between
the soft and hard tiers.

## Grace window

```sh
processkit-cli run \
  --timeout 20m \
  --grace 5s \
  --jsonl run.jsonl \
  -- service
```

The runner first asks the tree to stop, waits up to the grace window, and then
hard-kills survivors through the owning container. `cleanup_finished` reports
whether a soft request was delivered, was unsupported, or failed. Its `shutdown`
object also records the pre-attempt `soft_stop_scope` and ProcessKit's
`ShutdownReport`: observed member counts, early drain, escalation, and elapsed time.

Grace is a maximum opportunity for cooperative exit, not a promise that every
platform has a signal capable of reaching the child.

## Local stop signals

The foreground runner catches:

| Platform | Sources |
| --- | --- |
| All | `Ctrl-C` where delivered to the runner |
| Unix | `SIGTERM`, `SIGHUP` |
| Windows | `Ctrl-Break`, console close, logoff, shutdown |

These use the local `CANCELLED` code (`107`). The `cancelled.source` field names
the exact source (`ctrl_c`, `sigterm`, `sighup`, `ctrl_break`, `ctrl_close`,
`ctrl_logoff`, or `ctrl_shutdown`).

An inherited Unix terminal can deliver `Ctrl-C` directly to the foreground
child group. In that case the runner may report a child signal exit rather than
a runner-owned `107`. Use the control plane when a supervisor needs one stable
outcome independent of terminal routing.

## Control-plane cancel

```sh
processkit-cli cancel --run-id build-42
```

`cancel` reaches the live runner over local IPC and requests the same soft →
grace → hard teardown. The run exits `CONTROL_CANCELLED` (`108`) and writes a
`cancelled` event whose source identifies the control command.

The client returns only after the runner acknowledges the command. An
acknowledgement means the run accepted the request, not that teardown has
already completed; use `wait` as the barrier.

## Immediate kill

```sh
processkit-cli kill --run-id build-42
```

`kill` skips soft stop and grace, hard-kills the container immediately, and
produces `CONTROL_KILLED` (`109`) plus a `killed` event. Use it when cooperative
shutdown is not wanted or after an external policy has already spent its grace
budget.

## Aggregate cancellation

```sh
processkit-cli cancel --all
processkit-cli wait --all --timeout 30s
```

`--all` takes one snapshot of entries confirmed live. It addresses each by
record identity and remembered endpoint, so duplicate run ids do not make the
aggregate ambiguous. A newly registered or initially unprobed run is outside
that invocation's target set.

The command prints a JSON array with per-target `accepted`, `already_gone`, or
`failed` status. Any unresolved target makes the aggregate exit `CONTROL`
(`103`); an empty snapshot is successful.

## Windows soft-stop behavior

A Job Object has no POSIX signal. ProcessKit first tries a best-effort soft
close of eligible windowed members. `run --windows-graceful-ctrl-break` also
launches the direct console child as an addressable console process-group leader,
so ProcessKit can send `CTRL_BREAK` before the grace window. The flag is a no-op
off Windows and conflicts with `--create-no-window`/`--detach` because those modes
have no shared console through which to deliver the event.

The runner probes `soft_stop_scope` before delivery and reports `none` when no
eligible target exists. It never labels the grace as served by the child when
nothing was delivered.

Windows gives a console-close handler only a short system deadline. For that
source the runner caps an excessive grace request so it can finish teardown and
write terminal events before the OS ends the process.

## Unix soft-stop behavior

The soft tier sends `SIGTERM` to the contained process group or cgroup members,
then hard-kills survivors after grace. A descendant that deliberately escapes a
POSIX process group is outside that weaker mechanism; the runner reports the
active mechanism so an adapter can decide whether that limitation is acceptable.

## Event ordering

A runner-imposed ending normally emits:

```text
timeout | cancelled | killed
cleanup_started
members_snapshot
cleanup_finished
runner_exit
```

Exact presence and ordering rules are normative in
[JSONL event schema](schema.md#ordering).

## See also

- [Exit-code contract](exit-codes.md) — numeric outcomes.
- [Live-run control plane](control-plane.md) — request/ack protocol.
- [Platform support](platform-support.md) — containment caveats.
- [Troubleshooting](troubleshooting.md) — failures and stale records.
