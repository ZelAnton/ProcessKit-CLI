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

The one invocation this does not describe is `run --detach`, which by definition
stops being the child's parent: it reports whether the run *started* and leaves the
child's own code to the run's `runner_exit` event. See "Detached runs" below.

## Runner-own failures

When the **runner itself** fails — before, around, or instead of running the
child — it exits with a code from a distinct, reserved band so that a runner
failure is not mistaken for a child result.

**Reserved band: `100`–`119` inclusive.**

| Code | Name              | Meaning                                                                                     |
|------|-------------------|---------------------------------------------------------------------------------------------|
| 100  | `USAGE`           | Invalid command line: unknown flag, missing required option, malformed value (including a bad `--timeout`/`--grace` duration), or bad subcommand form. |
| 101  | `SPAWN`           | The target program could not be started (not found, not executable, bad `--cwd`, permission denied). |
| 102  | `BACKEND`         | ProcessKit backend/containment failure: kernel container, job object, IPC endpoint, or run registry could not be established — including a requested resource limit (`--max-memory` / `--max-processes` / `--cpu-quota`) the active mechanism could not apply (the machine-readable `limit_hit` event names which one; see "Resource limits" below). |
| 103  | `CONTROL`         | A by-`run-id` command could not be resolved to **the** single live run it names. For `inspect` / `cancel` / `kill` that covers every way the target cannot be reached: no such run id, a stale/dead registry entry, an entry whose liveness could not be probed at all (reported as `unprobed` — a refusal, not a claim that the runner died), an ambiguous run id (more than one live run registered under it), or an IPC failure. The registry-only `wait` shares exactly one of those reasons — an **ambiguous** run id — and reports it with this same code even though it contacts no runner: there is no single run to wait for. `cancel --all` / `kill --all` reuse this same code for a different fact — one or more record-addressed targets in the confirmed-live snapshot remained potentially live but could not be reached safely or did not acknowledge the command. A target confirmed gone before dispatch is instead the successful `already_gone` report status. The per-target reason is in the JSON report on stdout, not just the stderr tally. See `docs/control-plane.md`, "`cancel --all` / `kill --all`". |
| 104  | `INTERNAL`        | Unexpected runner fault: the runner reached a state its own logic rules out, or lost a trustworthy view of the run (a `wait` on the child failed and its fate is unknown; the backend returned an outcome this build cannot render). Reported with this code instead of panicking. **A genuine runner bug** — an ordinary setup failure is `SETUP` (111), not this. |
| 105  | `NOT_IMPLEMENTED` | **Retired.** Formerly minted for a defined-but-not-yet-built code path; every subcommand is now implemented, so no active path mints it. The number stays permanently reserved (see "Stability" below) — it is never reused for a different meaning. |
| 106  | `TIMEOUT`         | The run exceeded a runner deadline — the whole-run `--timeout`, or the `--idle-timeout` (the child went silent past the idle window) — and the runner tore the process tree down. A runner-*imposed outcome*, not a child exit. The two are told apart by the `timeout` event's `reason` (`overall` / `idle`), not by the code; both reuse `106` (see "Timeout, cancel, and kill" below). |
| 107  | `CANCELLED`       | The run was cancelled by a **local stop signal** — an interactive `Ctrl-C`, on Unix a `SIGTERM` / `SIGHUP` (a `kill`, a `systemctl stop`, a cancelled CI job, a hung-up terminal), or on Windows a `Ctrl-Break` / console close / logoff / system shutdown — and the runner tore the process tree down. The signals share this one code (the same class of ending); *which* one arrived is named by the `cancelled` event's `source` (`ctrl_c` / `sigterm` / `sighup` / `ctrl_break` / `ctrl_close` / `ctrl_logoff` / `ctrl_shutdown`). Distinct from `TIMEOUT` and from any child result. |
| 108  | `CONTROL_CANCELLED` | The run was cancelled by a control-plane `cancel` command (over the local control channel): the runner ran the same soft-stop → grace → hard-kill teardown as a Ctrl-C. Distinct from `CANCELLED` so "a control client cancelled it" is told from "a local signal stopped it". |
| 109  | `CONTROL_KILLED`  | The run was killed by a control-plane `kill` command: the runner hard-killed the whole tree immediately (no soft stop, no grace). Distinct from every other runner-imposed ending. |
| 110  | `PROBE_INCOMPATIBLE` | The **preflight probe** (`processkit-cli probe`) found this binary's compatibility surface does not satisfy a `--require-*` expectation. A *pre-launch* verdict, not a run outcome — no child is ever spawned by a probe. See "Preflight probe" below. |
| 111  | `SETUP`           | A fail-closed **setup / support failure**: a prerequisite the runner needs to run — or to report a result — could not be established or produced for an ordinary reason (its async runtime would not build, a required `--jsonl`/`--capture-dir` output or `--stdin-file` input could not be opened, or a `probe`/`inspect`/control reply would not serialize). An environment/resource condition the caller can usually act on (a bad path, missing permissions, exhausted resources), **not** a runner bug — that stays `INTERNAL` (104). See "Setup failures vs internal faults" below. |
| 112  | `WAIT_TIMEOUT`    | The **`wait` subcommand's own** deadline (`wait --run-id <id> --timeout <duration>`) elapsed while the run it was waiting for was still live. *The waiter* gave up; the run was never touched — `wait` is read-only and reaches no runner — and is still going. Deliberately **not** `TIMEOUT` (106), which means the opposite (the *runner* enforced a deadline and tore the child's tree down), and not `CONTROL` (103), since the run was resolved unambiguously and found perfectly healthy. See "A waiter's deadline is not a run's deadline" below. |

Codes `113`–`119` are **reserved** for future runner-own conditions. `--help`
and `--version` are not failures: they print to stdout and exit `0`.

## Timeout, cancel, and kill: runner-imposed outcomes

`TIMEOUT` (106), `CANCELLED` (107), `CONTROL_CANCELLED` (108), and `CONTROL_KILLED`
(109) are not *failures* of the runner and not the child's own exit — they are
outcomes the runner **imposes** when it ends a run that did not stop on its own. The
child did not choose to exit, so forwarding "its" code would be a lie; instead each
takes a distinct reserved-band code so a caller can tell them apart:

- the child exited by itself (its exact code, forwarded — possibly `0`),
- the runner ended it because a deadline elapsed — the whole-run `--timeout`, or the
  `--idle-timeout` (no child output for the idle window) — both `106`, told apart by
  the `timeout` event's `reason` (`overall` / `idle`) rather than by a distinct code,
- the runner ended it because a **local stop signal** reached it — the operator pressed
  `Ctrl-C`, on Unix a `SIGTERM`/`SIGHUP` arrived, or on Windows a `Ctrl-Break` / console
  close / logoff / system shutdown arrived (`107` for all of them, told apart by
  the `cancelled` event's `source` rather than by a distinct code),
- a control-plane `cancel` command ended it — the same graceful teardown as a Ctrl-C,
  but triggered over the network (`108`), and
- a control-plane `kill` command force-killed it immediately, no grace (`109`).

The two control-plane codes are what make a *remote* end-of-run distinguishable from a
*local* one, and a graceful `cancel` from an immediate `kill` — by code alone, before
even reading the event stream.

Alongside the code, the runner writes an explanatory line to **stderr** (never the
child's stdout) that also states, truthfully, how the tree was torn down — including
that on Windows a soft stop can only reach a windowed member, so for the ordinary
console child no soft stop is delivered at all, the grace window elapses, and the Job
Object is then killed atomically (see `README.md`, "Timeouts, cancel, and
grace"). As with every runner-own code, the numeric value is a best-effort signal;
the authoritative, machine-readable form of these outcomes is the `timeout` /
`cancelled` / `killed` event (and the terminal `runner_exit`) in the versioned JSONL
stream — see `docs/schema.md`.

## A waiter's deadline is not a run's deadline

`WAIT_TIMEOUT` (112) is the one code in this table that describes **the client**, not
the run. It is minted only by `wait --timeout <duration>` — targeting one run
(`--run-id <id>`) or, in aggregate, every run confirmed live in a snapshot taken at the
start (`--all`) — (see [`docs/registry.md`](registry.md), "Waiting — `wait`"), and only
for one situation: the wait deadline elapsed while its target(s) were still live, so
the command stopped waiting.

The distinction from `TIMEOUT` (106) is the whole reason it exists, and the two must
never be conflated:

- **`TIMEOUT` (106)** is reported by the *run's own process*: the runner enforced
  `--timeout`/`--idle-timeout` and tore the child's process tree down. The run is over,
  and it ended because of the deadline.
- **`WAIT_TIMEOUT` (112)** is reported by a *separate, read-only `wait` process*: it
  gave up watching. Nothing was sent to the runner — `wait` never connects to the
  control transport — so the run is unaffected, still running, and will end (and report
  its own outcome, with its own exit code and `runner_exit` event) whenever it does.

Nor is it a `CONTROL` (103) failure: nothing was unreachable or ambiguous — the run was
resolved to exactly one live entry and found perfectly healthy — so reporting "could not
reach the run" would be false. A caller that hits `112` has learned one fact and only
one: *the run had not finished yet*. Retrying the same `wait` is a perfectly reasonable
response to it, unlike a `103`, which will keep failing until the registry state changes.

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
  build; a required `--jsonl` events file, `--capture-dir`, or `--stdin-file` the
  operator asked for but that cannot be opened or created (an unwritable path, a missing
  parent, denied permissions); and a
  `probe` / `inspect` / control (`cancel`/`kill`) reply that cannot be serialized. It also
  covers the two failures that belong to `--detach`'s wrapper rather than to the run — the
  detached runner could not be spawned, or it never reported a started run before the
  startup budget elapsed (see "Detached runs" below). In every
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

## Resource limits reuse `BACKEND` (102)

When `run` is given a whole-tree resource cap (`--max-memory`, `--max-processes`,
or `--cpu-quota`) that the active containment mechanism cannot apply, the run ends
with **`BACKEND` (102)** and a `runner_exit` `source` of `container_error` — the
same code as any other container-creation failure — **not** a new code from the
reserved band's free slots. This is a deliberate choice: the failure is genuinely
that a *whole-tree container capable of the requested cap could not be established*
here (macOS/BSD and the Linux process-group fallback have no such container at all;
a Linux cgroup v2 whose controllers can't be enabled — under systemd, an ordinary
container, or typical CI — can't carry it either), which is the same class as the
existing container-creation error. The failure is always **pre-spawn**, so no child
code is ever at stake.

What makes a limit ending *distinguishable* is not the exit code but the dedicated
**`limit_hit`** JSONL event that precedes the `container_failed`/`runner_exit` tail
and names which limit (`memory` / `processes` / `cpu`) — the authoritative,
machine-readable channel, exactly as this document's core principle holds the exit
code to be only a best-effort hint (see "Why a band is not enough on its own"
below). A nonsensical value (`--max-memory 0`, a non-positive/non-finite
`--cpu-quota`) is instead a `USAGE` (100) argument error, rejected at parse time
before any container is touched. No reserved-band slot was spent on it.

## Detached runs: the code reports the start

`run --detach` is the one invocation where "the runner's exit code is the child's exit
code" does not apply, and it is not an exception carved out of the rule so much as a
consequence of the mode: the run is handed to a **detached copy** of the binary, so the
process the caller is waiting on has no child of its own to forward a code from. It
reports the only thing it can honestly report — whether the run *started*:

| Exit | Meaning |
| --- | --- |
| `0` | The run started: the detached runner registered it and wrote `run_started` to `--jsonl`. It is now discoverable (`list`), reachable (`inspect`/`cancel`/`kill`), and waitable (`wait`). Says nothing about how the child will finish. |
| a reserved-band code | The run did **not** start, and the code is the same one the failure would have produced in the foreground. |

**No new code was minted for this.** A start can fail in exactly the ways a foreground
run can fail before it begins — a program that is not there (`SPAWN` 101), a container
that cannot be created or a limit that cannot be applied (`BACKEND` 102), an unwritable
`--jsonl`/`--capture-dir` (`SETUP` 111) — and the detached copy exits with precisely
that code, which the caller then relays unchanged. A caller therefore reads `run
--detach`'s failures with the same table as `run`'s, and the reserved range `113`–`119`
stays free. Two failures belong to the detach wrapper itself rather than to the run, and
both take `SETUP` (111): the detached copy could not be spawned at all (a support step
failed; blaming `SPAWN` would point at the caller's program, which was never reached),
and the detached copy was alive but had not reported a started run before the startup
budget elapsed (it is killed rather than left running unreported). An exit status that
is *not* a reserved-band code — including a `0` — is likewise reported as `SETUP` and
never relayed, because no run path can exit successfully without having written
`run_started` first.

**What the events file holds after a failed start.** Whatever the detached copy managed
to write, and nothing invented on its behalf. A copy that started and *then* failed
records the failure itself — a `spawn_failed`/`container_failed` and a terminal
`runner_exit` with the matching `source`, exactly as a foreground run would — so the
stream explains the code the caller saw. The two wrapper failures above never reach that
point: the caller's own stderr and exit code are the entire account, and the events file
is left empty (or, if the copy was killed mid-startup, without a terminal `runner_exit`).
A stream with no `runner_exit` therefore means "no run started here", never "a run whose
ending was lost".

**Where the child's code went.** Nowhere: it is in the terminal `runner_exit` event of
the run's own `--jsonl` stream, with `code`, `source`, and `child_code` exactly as for
any other run. That is the whole trade of detaching — the caller gave up being the
runner's parent, so the process-exit channel for the child's result went with it, and
the event stream (which the `0` above guarantees exists and has begun) is the channel
that remains. `wait --run-id <id>` blocks until such a run is over, but it too reports
only *that* it ended, never with what code; the `runner_exit` event is the single source
for that.

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
  rather than overloading an existing one. `WAIT_TIMEOUT` (112) is the most recent,
  taking the next free slot after `SETUP` (111) rather than overloading `TIMEOUT`
  (106), whose meaning is the opposite one (see "A waiter's deadline is not a run's
  deadline" above); codes `113`–`119` remain reserved.
