# Exit-code contract

The runner's exit codes are part of processkit-cli's **public compatibility
surface**, alongside the CLI flags and the JSONL `schema_version` (see
`AGENTS.md`). Consumers — Orchestra and adapters such as processkit-py — depend
on these codes, so changing them incompatibly is a **major** version bump.

The in-code source of truth for these values is `src/exit.rs`; this document is
the normative description that external consumers pin against.

## The core rule: child fidelity

> The runner's exit code **is** the child's exit code.

On a completed run, processkit-cli exits with the exact code the child process
returned — unchanged, unclamped, un-aliased. Nothing in the runner rewrites a
child's `0`, its `1`, or its `137`. This is what makes the CLI a faithful,
transparent wrapper: a caller can branch on the child's status exactly as if it
had launched the child directly.

## Runner-own failures

When the **runner itself** fails — before, around, or instead of running the
child — it exits with a code from a distinct, reserved band so that a runner
failure is not mistaken for a child result.

**Reserved band: `100`–`119` inclusive.**

| Code | Name              | Meaning                                                                                     |
|------|-------------------|---------------------------------------------------------------------------------------------|
| 100  | `USAGE`           | Invalid command line: unknown flag, missing required option, malformed value (including a bad `--timeout`/`--grace` duration), or bad subcommand form. |
| 101  | `SPAWN`           | The target program could not be started (not found, not executable, bad `--cwd`, permission denied). |
| 102  | `BACKEND`         | ProcessKit backend/containment failure: kernel container, job object, IPC endpoint, or run registry could not be established. |
| 103  | `CONTROL`         | An `inspect` / `cancel` / `kill` command could not reach its target run: no such run id, a stale/dead registry entry, or an IPC failure. |
| 104  | `INTERNAL`        | Unexpected runner fault (an invariant was violated). Reported with this code instead of panicking. |
| 105  | `NOT_IMPLEMENTED` | A defined-but-not-yet-built code path. **Transitional** — present only while the runner is being implemented, and retired as each path lands. |
| 106  | `TIMEOUT`         | The run exceeded its `--timeout`: the runner enforced the deadline and tore the process tree down. A runner-*imposed outcome*, not a child exit. |
| 107  | `CANCELLED`       | The run was cancelled interactively (`Ctrl-C`): the runner tore the process tree down. Distinct from `TIMEOUT` and from any child result. |

Codes `108`–`119` are **reserved** for future runner-own conditions. `--help`
and `--version` are not failures: they print to stdout and exit `0`.

## Timeout and cancel: runner-imposed outcomes

`TIMEOUT` (106) and `CANCELLED` (107) are not *failures* of the runner and not the
child's own exit — they are outcomes the runner **imposes** when it ends a run that
did not stop on its own. The child did not choose to exit, so forwarding "its" code
would be a lie; instead each takes a reserved-band code so a caller can tell three
things apart:

- the child exited by itself (its exact code, forwarded — possibly `0`),
- the runner ended it because the `--timeout` deadline elapsed (`106`), and
- the runner ended it because the operator pressed `Ctrl-C` (`107`).

Alongside the code, the runner writes an explanatory line to **stderr** (never the
child's stdout) that also states, truthfully, how the tree was torn down — including
that on Windows there is no soft-terminate tier yet, so the grace window elapses and
the Job Object is then killed atomically (see `README.md`, "Timeouts, cancel, and
grace"). As with every runner-own code, the numeric value is a best-effort signal;
the authoritative, machine-readable form of these outcomes is the `timeout` /
`cancelled` event (and the terminal `runner_exit`) in the versioned JSONL stream —
see `docs/schema.md`.

## Why a band is not enough on its own

Exit codes are a single small integer, and a child can, in principle, exit with
a number that happens to fall inside `100`–`119` too. The reserved band is
therefore a best-effort signal for shells and scripts, **not** the authoritative
channel. The authority is the JSONL event stream: every run ends with a
`runner_exit` event (defined by the JSONL schema — see `docs/schema.md`) that
carries the returned code, names why the runner exited, and preserves the child's
own code in a separate `child_code` field, so a consumer reading `--jsonl` can
always tell a runner failure apart from a child that merely exited with the same
number. A child's own code is never lost or aliased, because the runner's failures
are additionally recorded out of band.

## Stability

- The **band** (`100`–`119`) and the **assigned codes** above are stable; moving
  or repurposing an assigned code is a breaking change.
- `NOT_IMPLEMENTED` (105) is the one intentionally temporary member: as the
  runner gains real behavior, the paths that return it are replaced, and it will
  eventually be unused. Its retirement is not a breaking change — it only ever
  meant "this build cannot do that yet."
- New runner-own conditions take the **next free code** in the reserved range
  rather than overloading an existing one.
