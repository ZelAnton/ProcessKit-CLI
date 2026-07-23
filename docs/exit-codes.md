# Exit-code contract

The runner's exit codes are part of processkit-cli's **public compatibility
surface**, alongside the CLI flags and the JSONL `schema_version` (see
`AGENTS.md`). Consumers and adapters such as processkit-py depend
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
| 103  | `CONTROL`         | An `inspect` / `cancel` / `kill` command could not reach its target run: no such run id, a stale/dead registry entry, an ambiguous run id (more than one live run registered under it), or an IPC failure. |
| 104  | `INTERNAL`        | Unexpected runner fault: the runner reached a state its own logic rules out, or lost a trustworthy view of the run (a `wait` on the child failed and its fate is unknown; the backend returned an outcome this build cannot render). Reported with this code instead of panicking. **A genuine runner bug** — an ordinary setup failure is `SETUP` (111), not this. |
| 105  | `NOT_IMPLEMENTED` | **Retired.** Formerly minted for a defined-but-not-yet-built code path; every subcommand is now implemented, so no active path mints it. The number stays permanently reserved (see "Stability" below) — it is never reused for a different meaning. |
| 106  | `TIMEOUT`         | The run exceeded its `--timeout`: the runner enforced the deadline and tore the process tree down. A runner-*imposed outcome*, not a child exit. |
| 107  | `CANCELLED`       | The run was cancelled interactively (`Ctrl-C`): the runner tore the process tree down. Distinct from `TIMEOUT` and from any child result. |
| 108  | `CONTROL_CANCELLED` | The run was cancelled by a control-plane `cancel` command (over the local control channel): the runner ran the same soft-stop → grace → hard-kill teardown as a Ctrl-C. Distinct from `CANCELLED` so "a control client cancelled it" is told from "the operator pressed Ctrl-C". |
| 109  | `CONTROL_KILLED`  | The run was killed by a control-plane `kill` command: the runner hard-killed the whole tree immediately (no soft stop, no grace). Distinct from every other runner-imposed ending. |
| 110  | `PROBE_INCOMPATIBLE` | The **preflight probe** (`processkit-cli probe`) found this binary's compatibility surface does not satisfy a `--require-*` expectation. A *pre-launch* verdict, not a run outcome — no child is ever spawned by a probe. See "Preflight probe" below. |
| 111  | `SETUP`           | A fail-closed **setup / support failure**: a prerequisite the runner needs to run — or to report a result — could not be established or produced for an ordinary reason (its async runtime would not build, a required `--jsonl`/`--capture-dir` output could not be created, or a `probe`/`inspect`/control reply would not serialize). An environment/resource condition the caller can usually act on (a bad path, missing permissions, exhausted resources), **not** a runner bug — that stays `INTERNAL` (104). See "Setup failures vs internal faults" below. |

Codes `112`–`119` are **reserved** for future runner-own conditions. `--help`
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
binary **before** launching anything through it. It is
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

## Setup failures vs internal faults

`SETUP` (111) and `INTERNAL` (104) are deliberately kept apart so the code alone tells a
caller which one happened:

- `SETUP` (111) is a **fail-closed setup / support failure**: the runner could not
  establish a prerequisite it needs, or produce a result it must emit, for an *ordinary*
  reason the caller can usually act on. It covers a `run` whose async runtime will not
  build; a required `--jsonl` events file or `--capture-dir` the operator asked for but
  that cannot be created (an unwritable path, a missing parent, denied permissions); and a
  `probe` / `inspect` / control (`cancel`/`kill`) reply that cannot be serialized. In every
  case the runner's own run-tracking logic is intact — a peripheral support step just
  failed — so reporting it as an `INTERNAL` "runner bug" would mislead the consumer. A
  `SETUP` failure before the child is spawned takes the `SETUP` code and (where a `--jsonl`
  stream is already open) a terminal `runner_exit` with `source: "setup"` and a null
  `child_code`; no child code is ever lost, because no child ran.
- `INTERNAL` (104) stays **strictly for a genuine invariant violation**: the runner
  reached a state its own logic rules out (the backend reported a `TimedOut` outcome when no
  deadline was armed on the child, or an outcome variant this build does not recognize), or
  lost a trustworthy view of the run it cannot recover from (a `wait` on the child failed
  and its fate is now unknown). These *are* runner bugs, and a consumer reading `104` can
  treat them as such.

The distinction is which side failed: an environment/resource condition the caller can fix
(`SETUP`) versus the runner's own logic being wrong (`INTERNAL`).

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
- `NOT_IMPLEMENTED` (105) was the one intentionally temporary member: it has now
  retired, since every subcommand it once stood in for is implemented. Its
  retirement was not a breaking change — it only ever meant "this build cannot
  do that yet" — but the number is not reassigned to a new meaning; it stays
  reserved and unused going forward.
- New runner-own conditions take the **next free code** in the reserved range
  rather than overloading an existing one. `SETUP` (111) is the most recent, taking
  the next free slot after `PROBE_INCOMPATIBLE` (110); codes `112`–`119` remain
  reserved.
