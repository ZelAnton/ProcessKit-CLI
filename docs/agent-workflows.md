# Running external tools from automation agents

An automation or coding agent can use `processkit-cli` as the execution boundary
for builds, tests, compilers, package managers, development servers, and other
external tools. The agent does not need a special SDK: it only needs to be
instructed to invoke the tool through the runner instead of starting it directly.

This is especially useful for commands that can spawn workers, become silent,
hold output handles open, or outlive the agent action that launched them.

## Why use a runner

A direct process launch gives an agent one process handle, but the real workload
may be a tree: a compiler can start helpers, a test host can start workers, and a
build tool can keep reusable nodes alive. Treating only the root PID as the unit
of cleanup creates several failure modes:

- stopping the agent or cancelling its tool call may leave descendants running;
- a descendant that retains stdout or stderr can make the caller wait forever;
- a lost process handle leaves the next agent turn with little reliable state;
- cleanup by process name can terminate unrelated work;
- a reused PID can make delayed cleanup target the wrong process;
- unbounded output and silent hangs consume time, disk, and compute without a
  useful diagnostic record.

ProcessKit CLI gives the agent a higher-level unit: one run with a contained
process tree, explicit deadlines, bounded transcripts, versioned lifecycle
events, and a run-id-based control plane.

| Agent concern | Runner capability |
| --- | --- |
| A tool spawns descendants | The ProcessKit container is the cleanup scope. |
| A command never finishes | `--timeout` bounds total runtime. |
| A worker becomes silent | `--idle-timeout` detects missing output activity. |
| Live output is too noisy | `--no-echo` suppresses relay while capture continues. |
| Output is needed after failure | `--capture-dir` keeps bounded stdout/stderr files with hashes and truncation metadata. |
| The agent loses its local process handle | `list`, `inspect`, and `wait` operate through the per-user run registry. |
| Cooperative stop fails | `cancel` escalates after grace; `kill` hard-kills the owned container immediately. |
| A caller needs machine-readable evidence | `--jsonl` records the versioned lifecycle and terminal outcome. |
| Several execution policies are needed | The agent can choose deadlines, capture, environment, resource limits, and foreground/detached supervision per task. |

## A drop-in instruction for an agent

The following policy can be placed in an `AGENTS.md`, system prompt, tool
description, or project automation guide. Adjust paths and default durations to
the repository:

> When launching an external command that may run for more than a few seconds,
> spawn descendants, or require reliable cleanup, execute it through
> `processkit-cli run` rather than starting it directly. Create a private,
> per-invocation run directory and a unique, non-secret run id. Pass the program
> and its arguments after `--` without a shell unless shell syntax is explicitly
> required. Always write JSONL events, set a total timeout, add an idle timeout
> when silence indicates a stuck tool, and use bounded capture when output may be
> needed for diagnosis. Prefer a foreground run. On cancellation or uncertainty,
> address the run by its run id with `inspect`, `cancel`, `wait`, and finally
> `kill` if graceful cancellation does not finish. Never clean up by process name
> or by a PID copied from earlier output. Use `--detach` only when a separate
> supervisor deliberately owns the continuing run.

The executable can be preflighted once per agent session:

```sh
processkit-cli probe --json \
  --require-schema-version 1 \
  --require-exit-code-band 100-119 \
  --require-surface run:--timeout \
  --require-surface run:--capture-dir \
  --require-surface cancel:--run-id
```

An agent can then adapt this invocation template to each tool:

```sh
# Create RUN_DIR first and choose a unique RUN_ID for this invocation.
processkit-cli run \
  --run-id "$RUN_ID" \
  --timeout 20m \
  --idle-timeout 3m \
  --grace 5s \
  --capture-dir "$RUN_DIR/capture" \
  --capture-max-bytes 16m \
  --jsonl "$RUN_DIR/events.jsonl" \
  -- <program> <args...>
```

The runner's exit code is the program's exit code on normal completion. The
documented `100`-`119` band is a useful first signal for runner outcomes, but a
child can coincidentally return a number in the same range. The terminal
`runner_exit` event is authoritative: its source and separate `child_code` let
the agent distinguish a failing test from a timeout, cancellation, spawn
failure, or backend failure without guessing from the number alone.

## A robust foreground strategy

Foreground execution is the safest default for an agent action:

1. Generate a unique run id and create its diagnostic directory.
2. Start `processkit-cli run` with an overall timeout and durable JSONL path.
3. Stream ordinary child output to the agent, or use `--no-echo` with bounded
   capture when live output would consume too much context.
4. Interpret the returned code together with terminal `runner_exit`.
5. If the tool call is cancelled, send a supported stop signal to the foreground
   runner and wait for its normal container teardown.
6. If completion becomes uncertain, use `list --json` and `inspect --json`
   rather than guessing from a PID.
7. Request `cancel`, wait for a bounded interval, and use `kill` only as the
   escalation step.
8. Retain JSONL and capture files for failed runs; discard them according to the
   project's artifact policy after success.

This makes retries safer because each attempt has a distinct identity and
diagnostic record. A later agent turn can tell whether an earlier attempt is
live, finished, stale, or unprobeable before deciding whether to retry.

## Strategy examples

### Build or test with a hard ceiling

```sh
mkdir -p .agent-runs/agent-test-42
processkit-cli run --run-id agent-test-42 \
  --timeout 15m --idle-timeout 2m \
  --capture-dir .agent-runs/agent-test-42/capture \
  --jsonl .agent-runs/agent-test-42/events.jsonl \
  -- cargo test --all-features
```

This protects the agent from both a long-running test suite and a build worker
that stops producing output.

### Keep verbose output out of the agent context

```sh
mkdir -p .agent-runs/agent-build-42
processkit-cli run --run-id agent-build-42 \
  --timeout 20m --no-echo \
  --capture-dir .agent-runs/agent-build-42/capture \
  --jsonl .agent-runs/agent-build-42/events.jsonl \
  -- cargo build --release
```

The agent can read the bounded transcript only when it needs to diagnose a
failure. JSONL still provides the lifecycle and terminal result.

### Recover after an interrupted agent turn

```sh
processkit-cli list --json
processkit-cli inspect --run-id agent-build-42 --json
processkit-cli cancel --run-id agent-build-42
processkit-cli wait --run-id agent-build-42 --timeout 30s
```

If the bounded wait expires and the run must stop immediately:

```sh
processkit-cli kill --run-id agent-build-42
```

Control commands resolve the registry identity and live IPC endpoint. They do
not target the root PID printed by an earlier observation.

### Apply a policy to expensive tools

Where the platform can enforce whole-tree limits, an agent can combine time and
resource budgets:

```sh
mkdir -p .agent-runs/agent-compiler-42
processkit-cli run --run-id agent-compiler-42 \
  --timeout 20m --max-memory 4g --max-processes 64 --cpu-quota 4 \
  --jsonl .agent-runs/agent-compiler-42/events.jsonl \
  -- compiler <args...>
```

Limit requests fail before spawn when the active backend cannot enforce them.
The agent should treat that as a policy failure, not silently rerun the command
without limits.

## What happens when the agent stops

The runner substantially improves cancellation and cleanup, but it is important
to state the boundary precisely:

- If the agent host cancels the foreground tool call by delivering a supported
  stop signal to `processkit-cli`, the runner performs its normal whole-container
  teardown.
- Overall and idle timeouts remain active inside a surviving runner, so work is
  bounded even if the agent no longer observes the call.
- The runner does not monitor an agent identity and cannot infer that an
  unrelated parent application has disappeared. If the host abandons the runner
  without terminating it, the run may continue until its own deadline or child
  completion.
- If the runner itself is killed before normal teardown, the `abrupt_cleanup`
  field reports the real platform guarantee: `whole_tree` on Windows,
  `direct_child_only` on Linux, and `none` on macOS/other Unix.
- A detached run deliberately outlives the launching call. It requires a durable
  JSONL path, a stable run id, deadlines, and a separate supervisor or recovery
  policy.

For a host that must guarantee cleanup when an agent session ends, combine the
runner with an explicit host policy: keep `run` in the foreground, terminate the
runner during agent teardown, require finite deadlines, and reconcile remaining
registry entries before the next session. Do not describe `--detach` as a leak
prevention mechanism.

## Diagnostic workflow for agents

When a command fails or appears stuck, an agent can collect evidence in this
order:

1. Read the `processkit-cli` exit code.
2. Read the final complete JSONL records, especially `runner_exit` and any
   preceding timeout, cancellation, spawn, container, or limit event.
3. Read bounded stdout/stderr capture only when the program's output is needed.
4. If the run may still be live, call `inspect --json` for the current member
   snapshot and containment mechanism.
5. Use `cancel` followed by bounded `wait`; escalate to `kill` only when needed.
6. Use `prune --dry-run --json` before removing confirmed-stale registry state.

This separates program failure, runner failure, policy failure, and ambiguous
external interruption instead of collapsing them into “the command hung.”

## See also

- [Cookbook](cookbook.md) — shorter copyable command recipes.
- [Running commands](running-commands.md) — argv, environment, and flag
  interactions.
- [Timeouts and cancellation](timeouts-and-cancellation.md) — deadline and
  escalation semantics.
- [Detached runs](detached-runs.md) — the intentionally out-of-band mode.
- [Integration guide](integration.md) — a complete adapter lifecycle.
- [Platform support](platform-support.md) — normal and abrupt cleanup strength.
