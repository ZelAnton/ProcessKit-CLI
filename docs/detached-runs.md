# Detached runs

`--detach` transfers a run to a detached copy of `processkit-cli` and returns
once that copy has provably started the child. It is intended for an
orchestrator that wants to launch now and supervise later from a different
process.

```sh
processkit-cli run \
  --detach \
  --run-id build-42 \
  --capture-dir ./build-42-output \
  --jsonl build-42.jsonl \
  -- cargo build

processkit-cli inspect --run-id build-42 --json
processkit-cli wait --run-id build-42
```

## What successful return proves

The launching invocation waits until `run_started` is readable in the JSONL
file. That event is written after the detached runner has:

1. created the ProcessKit container;
2. published the registry record and control endpoint;
3. spawned the child.

After a successful return, the run is discoverable by `list`, addressable by
`inspect` / `cancel` / `kill`, and waitable by `wait`. A timeout in the detached
startup handshake kills the detached copy instead of leaving an unreported run
behind.

## Exit-code semantics change

Foreground `run` forwards the child's exact exit code. Detached launch cannot:
the launching process exits while the child is still running.

| Detached launch result | Launcher exit code |
| --- | --- |
| Run reached `run_started` | `0` |
| Program could not spawn | `SPAWN` (`101`) |
| Container could not be created | `BACKEND` (`102`) |
| JSONL or setup failed | `SETUP` (`111`) or the applicable reserved code |

The child's eventual result is in the terminal `runner_exit` event. A detached
caller must treat `0` as “started,” never as “payload succeeded.” See
[Exit-code contract](exit-codes.md#detached-runs-the-code-reports-the-start).

## Output behavior

The detached runner owns none of the launching caller's standard handles. It
uses the same pump as `--no-echo`: child output is drained, but not retransmitted
to a terminal that no longer exists.

These features remain active:

- `--jsonl` (still required);
- `--capture-dir` and `--capture-max-bytes`;
- `--idle-timeout`;
- overall timeout, grace, environment, cwd, and resource limits.

`--inherit-stdio` and `--inherit-stdin` are rejected because there is no caller
left to provide those handles. Use `--stdin-file` when a detached payload needs
finite input.

## Windows

The detached runner is created with `DETACHED_PROCESS` and owns no console.
A console child may therefore receive a new visible console from Windows. Pass
`--create-no-window` for a headless payload:

```powershell
processkit-cli run --detach --create-no-window `
  --run-id worker `
  --jsonl worker.jsonl `
  -- worker.exe
```

A detached runner cannot escape a Job Object that already contains its caller.
If the outer job is kill-on-close, its owner still controls the detached run's
ultimate lifetime.

## Unix

The detached copy starts a new session with `setsid`, so terminal hang-up and
`Ctrl-C` from the launching session no longer target it. After the launcher
exits, the system's init process adopts the detached runner.

This changes session ownership, not ProcessKit's containment mechanism. The
run still reports `cgroup_v2` or `process_group`, and the platform-specific
`abrupt_cleanup` value still applies if the detached runner itself is killed
before it can perform teardown.

## Supervision pattern

Use one durable JSONL path and one stable run id per detached run:

```sh
run_id=nightly-index
events="/var/lib/my-orchestrator/runs/${run_id}.jsonl"

processkit-cli run --detach --run-id "$run_id" --jsonl "$events" -- indexer
processkit-cli wait --run-id "$run_id" --timeout 30m
```

The run id is an address while the runner is live. The JSONL file is the durable
record after the registry entry has been removed on clean completion.

## Shutdown sequence

For one detached run:

```sh
processkit-cli cancel --run-id nightly-index
processkit-cli wait --run-id nightly-index --timeout 30s
processkit-cli prune
```

For all runs confirmed live at one instant:

```sh
processkit-cli cancel --all
processkit-cli wait --all --timeout 30s
processkit-cli prune --dry-run
processkit-cli prune
```

The `--all` target set is a snapshot. A run registered afterward is out of
scope; repeat the sequence when the surrounding system allows concurrent new
launches.

## Recovery after orchestrator restart

1. Run `list --json` to discover registry entries.
2. Tail each durable JSONL file from its last complete line.
3. Use `inspect --json` only for entries confirmed live.
4. Treat stale records as evidence of abrupt runner loss, not as permission to
   address the recorded PID.
5. Preview `prune --dry-run --json`, then prune confirmed-stale entries.

## See also

- [Run registry](registry.md) — liveness and stale-entry semantics.
- [Live-run control plane](control-plane.md) — control acknowledgements.
- [Integration guide](integration.md) — adapter startup and recovery.
- [Platform support](platform-support.md) — abrupt-runner-death guarantees.
