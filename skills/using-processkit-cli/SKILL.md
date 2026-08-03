---
name: using-processkit-cli
description: Run external build, test, tool, or service commands through ProcessKit CLI when descendants must not become orphans, execution needs a hard or idle deadline, output needs bounded capture, or an automation agent needs JSONL lifecycle evidence and out-of-band supervision. Use for potentially long-lived or process-spawning commands. Do not use for shell built-ins, simple file reads, or interactive TTY programs that need a PTY.
---

# Use ProcessKit CLI

Prefer `processkit-cli run` over launching a risky external command directly. Keep
the invocation shell-free: every token after `--` is the program and its argv. If
shell syntax is genuinely required, pass `sh -c` or `cmd /c` explicitly.

## Preflight

Fail closed before the first payload when the runner may be missing or stale:

```sh
processkit-cli probe --json \
  --require-schema-version 1 \
  --require-exit-code-band 100-119 \
  --require-surface run:--jsonl \
  --require-surface run:--timeout
```

Add one `--require-surface` for every optional flag the workflow depends on.

## Run and observe

Use absolute artifact paths when another process must read them. Always impose a
realistic ceiling; add `--idle-timeout` when silence itself means the tool is stuck.

```sh
processkit-cli run \
  --run-id build-42 \
  --jsonl /absolute/run/events.jsonl \
  --timeout 30m --idle-timeout 5m --grace 10s \
  -- program arg1 arg2
```

Child stdout and stderr remain separate and echo live. To retain bounded logs while
suppressing the echo, add:

```text
--capture-dir /absolute/run/capture --capture-max-bytes 8m --no-echo
```

For a noisy runaway, add `--capture-overflow cancel`; it gracefully ends the run
when either capture stream exceeds its ceiling. On headless Windows runs, add
`--create-no-window` unless the child deliberately needs a real console.

After the process returns, parse the final complete JSONL line. Treat
`runner_exit.source` and nullable `child_code` as authoritative; the numeric shell
status alone is ambiguous because a child can itself return a number in the
runner-owned `100-119` band.

Runner-imposed outcomes include `TIMEOUT (106)`, local-signal `CANCELLED (107)`,
control-plane cancel `CONTROL_CANCELLED (108)`, immediate control-plane kill
`CONTROL_KILLED (109)`, and output-volume protection `OUTPUT_OVERFLOW (113)`.
`WAIT_TIMEOUT (112)` means only that a separate waiter stopped waiting; it does not
end the run.

## Detach and supervise

Use detach only with durable absolute JSONL/capture paths and a unique run id:

```sh
processkit-cli run --detach --run-id build-42 \
  --jsonl /absolute/run/events.jsonl --timeout 30m \
  -- program arg1 arg2
processkit-cli wait --run-id build-42 --timeout 35m
processkit-cli inspect --run-id build-42 --json
```

The detach command's `0` means the run started, not that the child succeeded. Read
the detached run's terminal `runner_exit` for its outcome — with the built-in
reader rather than an ad-hoc `tail`/`jq`:

```sh
processkit-cli events --run-id build-42 --follow        # watch it happen
processkit-cli events --file /absolute/run/events.jsonl  # after the fact
```

`events` is read-only and resolves the stream through the registry (`--run-id`) or
directly (`--file`, once the registry record is gone). `--follow` returns at the
terminal `runner_exit`, or when the run is over and the stream stopped growing.
`--json` passes the runner's own lines through byte for byte; `--validate` checks a
stream against the embedded event schema and exits `EVENTS_INVALID (114)` if any
line does not conform.

Use `cancel --run-id build-42` for graceful teardown and `kill --run-id build-42`
only for an immediate hard kill. Never clean up by process name or PID. For fleets,
use the built-in `list`, `inspect --all --json`, `cancel --all`, `kill --all`, and
`wait --all` forms with exact `--label KEY=VALUE` filters.
