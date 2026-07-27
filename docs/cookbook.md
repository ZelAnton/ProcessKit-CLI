# Cookbook

Task-oriented command shapes for ProcessKit CLI. Each recipe keeps the JSONL
destination explicit and places the child after `--`, so the boundary between
runner options and child argv remains visible.

## Run a command and preserve its exit code

```sh
processkit-cli run --jsonl run.jsonl -- cargo test
```

For a foreground run, the CLI exits with the child's exact code after tearing
down the contained tree. Runner-owned failures and cancellations use the
reserved band; read [Exit-code contract](exit-codes.md) when the caller must
distinguish them from a child that returned the same number.

## Run in another directory

```sh
processkit-cli run \
  --cwd ./services/catalog \
  --jsonl catalog-test.jsonl \
  -- cargo test --locked
```

`run_started.cwd` records the resolved absolute directory.

## Start from a controlled environment

```sh
processkit-cli run \
  --env-clear \
  --env PATH=/usr/bin:/bin \
  --env HOME=/tmp/worker-home \
  --env CI=true \
  --jsonl hermetic.jsonl \
  -- worker
```

Use absolute program paths when clearing `PATH` entirely. Applied order is
clear → remove → set.

## Remove one inherited secret

```sh
processkit-cli run \
  --env-remove GITHUB_TOKEN \
  --env-remove AWS_SECRET_ACCESS_KEY \
  --jsonl sanitized.jsonl \
  -- third-party-tool
```

Environment values are not placed in JSONL, but the child can still echo them.

## Capture output without echoing it live

```sh
processkit-cli run \
  --no-echo \
  --capture-dir ./capture \
  --capture-max-bytes 16m \
  --jsonl captured.jsonl \
  -- noisy-build
```

Read `output_captured.truncated` before treating either file as complete.

## Feed a finite input file

```sh
processkit-cli run \
  --stdin-file request.json \
  --capture-dir ./response \
  --jsonl request-run.jsonl \
  -- json-transform
```

The file closes the child's stdin at EOF and its bytes never enter argv.

## Run an interactive terminal program

```sh
processkit-cli run --inherit-stdio --jsonl interactive.jsonl -- repl-tool
```

The child sees the caller's existing terminal. Capture, no-echo, idle timeout,
detach, and `--create-no-window` are unavailable in this mode. This preserves a
terminal; it does not create a PTY.

## Bound total runtime

```sh
processkit-cli run \
  --timeout 15m \
  --grace 10s \
  --jsonl timed.jsonl \
  -- integration-tests
```

Expiry emits `timeout` with `reason: "overall"`, then the cleanup sequence and
terminal `runner_exit`.

## Kill a worker that stops producing output

```sh
processkit-cli run \
  --idle-timeout 2m \
  --grace 5s \
  --jsonl worker.jsonl \
  -- build-worker
```

Every observed stdout/stderr chunk re-arms the idle clock. Use only for tools
whose silence is a meaningful health signal.

## Give no soft-stop grace

```sh
processkit-cli run --timeout 30s --grace 0 --jsonl fast.jsonl -- disposable-task
```

`0` is legal for grace and means immediate progression to the hard tier. It is
rejected for overall, idle, and wait deadlines.

## Launch out of band and supervise later

```sh
processkit-cli run \
  --detach \
  --run-id nightly-build \
  --capture-dir ./nightly-output \
  --jsonl nightly.jsonl \
  -- cargo build --release

processkit-cli inspect --run-id nightly-build
processkit-cli wait --run-id nightly-build --timeout 30m
```

The detached launcher's `0` means “started.” Read terminal JSONL for the child's
eventual result.

## Inspect a live tree as JSON

```sh
processkit-cli inspect --run-id nightly-build --json
```

The snapshot includes the mechanism, root pid, start time, and current members
with nullable enriched fields. It is an observation at request time, not a
durable history.

## Ask one run to stop cooperatively

```sh
processkit-cli cancel --run-id nightly-build
processkit-cli wait --run-id nightly-build --timeout 30s
```

`cancel` acknowledges the request; `wait` is the completion barrier.

## Hard-kill one run now

```sh
processkit-cli kill --run-id wedged-worker
processkit-cli wait --run-id wedged-worker --timeout 10s
```

This skips soft stop and grace and produces a distinct `killed` outcome.

## Shut down every currently live run

```sh
processkit-cli cancel --all
processkit-cli wait --all --timeout 30s
processkit-cli prune --dry-run
processkit-cli prune
```

Both `--all` operations use their own snapshots. Prevent new launches during a
global shutdown or repeat the sequence to catch later registrations.

## Discover runs without knowing their ids

```sh
processkit-cli list
processkit-cli list --json
```

The human table abbreviates `argv_sha256`; JSON Lines carry the full digest.
`live`, `stale`, and `unprobed` are intentionally distinct health states.

## Preview stale-record cleanup

```sh
processkit-cli prune --dry-run --json
```

Only entries whose liveness probe succeeded and reported stale appear as
candidates. `unprobed` entries are preserved.

## Verify a runner before using it

```sh
processkit-cli probe --json \
  --require-schema-version 1 \
  --require-exit-code-band 100-119 \
  --require-surface run:--capture-dir \
  --require-surface cancel:--all \
  --require-surface wait:--all
```

Exit `110` means the binary is incompatible with at least one requirement. No
child or registry entry is created.

## Export the exact event schema

```sh
processkit-cli probe --json --print-schema > processkit-cli.schema.json
```

This prints the schema embedded in that exact binary, which is useful when the
consumer has an installed executable but no matching git checkout.

## Require whole-tree resource caps

```sh
processkit-cli run \
  --max-memory 2g \
  --max-processes 64 \
  --cpu-quota 2 \
  --jsonl limited.jsonl \
  -- untrusted-compiler
```

Unsupported enforcement fails before spawn with `limit_hit`; it never runs the
child without the requested policy. See [Resource limits](resource-limits.md)
before using this in Linux containers or systemd.

## Hide a detached Windows console

```powershell
processkit-cli run --detach --create-no-window `
  --run-id headless-worker `
  --jsonl headless.jsonl `
  -- worker.exe
```

Use this only for a child that does not require a console. The runner never
forces `CREATE_NO_WINDOW` by default.

## Invoke a shell explicitly

```sh
processkit-cli run --jsonl shell.jsonl -- sh -c 'make all && make test'
```

```powershell
processkit-cli run --jsonl shell.jsonl -- `
  pwsh -NoProfile -Command 'Get-ChildItem Env: | Sort-Object Name'
```

The shell is now an explicit child program. Its quoting, expansion, and pipeline
semantics are outside ProcessKit CLI.

## Keep child stdout machine-clean

```sh
processkit-cli run --jsonl events.jsonl -- report-generator > report.bin
```

JSONL never goes to stdout. Runner diagnostics use stderr, and child stderr is
also forwarded there. Use `--no-echo --capture-dir` when stdout must not be
forwarded at all.

## Tail lifecycle events safely

Treat JSONL as an append-only sequence of complete lines. A reader should:

1. buffer until newline;
2. parse one object;
3. verify `schema_version`;
4. dispatch on `type` while tolerating unknown additive fields;
5. stop only after terminal `runner_exit` or an explicit external recovery
   decision.

The file may end with a partial line if the runner is killed during a write.
Do not parse that suffix as a complete event.

## Recover after the supervising application restarts

```sh
processkit-cli list --json
processkit-cli inspect --run-id recovered-run --json
processkit-cli wait --run-id recovered-run --timeout 30s
```

Use the registry for current liveness and the JSONL file for durable history.
Never reconnect by recorded PID.

## Give an automation agent a bounded execution policy

Instruct the agent to launch external tools through a foreground runner with a
unique run id, finite deadlines, lifecycle JSONL, and bounded capture:

```sh
mkdir -p .agent-runs/agent-task-42
processkit-cli run --run-id agent-task-42 \
  --timeout 20m --idle-timeout 3m \
  --capture-dir .agent-runs/agent-task-42/capture \
  --jsonl .agent-runs/agent-task-42/events.jsonl \
  -- <program> <args...>
```

The agent should cancel and wait by run id, never clean up by PID or process
name, and reserve `--detach` for work with a separate supervisor. See
[Agent and automation workflows](agent-workflows.md) for a ready-to-paste
instruction, recovery strategy, and the precise limits of cleanup when the
agent itself stops.

## Use as a container entrypoint

```dockerfile
ENTRYPOINT ["/usr/local/bin/processkit-cli", "run", "--jsonl", "/run/events.jsonl", "--"]
CMD ["/app/worker"]
```

Exec form preserves signal delivery and avoids a shell wrapper. Ensure `/run`
is writable and the orchestrator's termination grace exceeds the CLI's grace.

## Diagnose a failed start

1. Read stderr for the operator message.
2. Read JSONL for `spawn_failed`, `limit_hit`, or `container_failed`.
3. Read terminal `runner_exit` for runner code and nullable child code.
4. If a registry entry remains after abrupt runner death, use `list` and
   `prune --dry-run`; do not kill the recorded pid.

## Guide map

| Need | Read |
| --- | --- |
| argv, cwd, environment | [Running commands](running-commands.md) |
| terminal, stdin, capture | [Standard I/O and capture](io-and-capture.md) |
| out-of-band lifecycle | [Detached runs](detached-runs.md) |
| deadlines and stop behavior | [Timeouts and cancellation](timeouts-and-cancellation.md) |
| memory/process/CPU caps | [Resource limits](resource-limits.md) |
| OS differences | [Platform support](platform-support.md) |
| agent tool execution | [Agent and automation workflows](agent-workflows.md) |
| adapter design | [Integration guide](integration.md) |
| event fields | [JSONL event schema](schema.md) |
