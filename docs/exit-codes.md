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
| 108  | `CONTROL_CANCELLED` | The run was cancelled by a control-plane `cancel` command (over the local control channel): the runner ran the same soft-stop → grace → hard-kill teardown as a Ctrl-C. Distinct from `CANCELLED` so "a control client cancelled it" is told from "the operator pressed Ctrl-C". |
| 109  | `CONTROL_KILLED`  | The run was killed by a control-plane `kill` command: the runner hard-killed the whole tree immediately (no soft stop, no grace). Distinct from every other runner-imposed ending. |
| 110  | `PROBE_INCOMPATIBLE` | The **preflight probe** (`processkit-cli probe`) found this binary's compatibility surface does not satisfy a `--require-*` expectation. A *pre-launch* verdict, not a run outcome — no child is ever spawned by a probe. See "Preflight probe" below and [env-launch.md](env-launch.md). |

Codes `111`–`119` are **reserved** for future runner-own conditions. `--help`
and `--version` are not failures: they print to stdout and exit `0`.

## Timeout, cancel, and kill: runner-imposed outcomes

`TIMEOUT` (106), `CANCELLED` (107), `CONTROL_CANCELLED` (108), and `CONTROL_KILLED`
(109) are not *failures* of the runner and not the child's own exit — they are
outcomes the runner **imposes** when it ends a run that did not stop on its own. The
child did not choose to exit, so forwarding "its" code would be a lie; instead each
takes a distinct reserved-band code so a caller can tell them apart:

- the child exited by itself (its exact code, forwarded — possibly `0`),
- the runner ended it because the `--timeout` deadline elapsed (`106`),
- the runner ended it because the operator pressed `Ctrl-C` (`107`),
- a control-plane `cancel` command ended it — the same graceful teardown as a Ctrl-C,
  but triggered over the network (`108`), and
- a control-plane `kill` command force-killed it immediately, no grace (`109`).

The two control-plane codes are what make a *remote* end-of-run distinguishable from a
*local* one, and a graceful `cancel` from an immediate `kill` — by code alone, before
even reading the event stream.

Alongside the code, the runner writes an explanatory line to **stderr** (never the
child's stdout) that also states, truthfully, how the tree was torn down — including
that on Windows there is no soft-terminate tier yet, so the grace window elapses and
the Job Object is then killed atomically (see `README.md`, "Timeouts, cancel, and
grace"). As with every runner-own code, the numeric value is a best-effort signal;
the authoritative, machine-readable form of these outcomes is the `timeout` /
`cancelled` / `killed` event (and the terminal `runner_exit`) in the versioned JSONL
stream — see `docs/schema.md`.

## Preflight probe: a pre-launch verdict, not a run outcome

`PROBE_INCOMPATIBLE` (110) is different in kind from every code above. It is not the
ending of a run — the `probe` subcommand never spawns a child, opens the registry, or
creates a container — but the verdict of a *preflight* a consumer runs on a candidate
binary **before** launching anything through it (see `docs/env-launch.md`). It is
minted only when the probe was asked to verify an expectation
(`--require-schema-version`, `--require-exit-code-band`, or `--require-surface`) that
this binary's surface does not satisfy. A satisfied (or unrequested) surface exits
`0`. The launcher contract is **fail-closed**: an incompatible binary must be reported
with this distinct, reserved code rather than silently used, so a consumer never
degrades into an uncontained launch. As with the run codes, the number is a
best-effort signal; the authoritative detail is the probe's JSON report (`compatible`
+ `mismatches`).

A malformed probe argument (for example a bad `--require-exit-code-band` value) is a
`USAGE` (100) error like any other bad flag — distinct from `PROBE_INCOMPATIBLE`, which
means "the arguments were well-formed, but this binary cannot meet them".

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
  rather than overloading an existing one. `PROBE_INCOMPATIBLE` (110) is the most
  recent, taking the next free slot after the control-plane endings; codes
  `111`–`119` remain reserved.
