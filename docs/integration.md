# Integration guide for adapters

This is the walkthrough for a **consumer** of `processkit-cli` — an orchestrator or
adapter (in particular the processkit-py CLI) that launches runs through this
binary and reads its results back — rather than for a contributor to this
repository (see [`docs/architecture.md`](architecture.md) for that audience). It
ties together, in the order an adapter actually exercises them, the five
normative documents that each cover one part of the compatibility surface on
their own: [`docs/schema.md`](schema.md), [`docs/exit-codes.md`](exit-codes.md),
[`docs/control-plane.md`](control-plane.md), and
[`docs/registry.md`](registry.md). This document does not restate their
normative text — every concrete claim below is a pointer to, and a minimal
worked example of, the contract those documents define; **on any disagreement,
the linked document is the source of truth.**

## 1. Fail-closed preflight: `probe`

Before launching anything through a candidate `processkit-cli` binary, verify
it is compatible. `probe` is side-effect-free — it spawns no child and touches
no registry or container — and prints one JSON report line to stdout:

```sh
processkit-cli probe --json \
  --require-schema-version 1 \
  --require-exit-code-band 100-119 \
  --require-surface run:--jsonl \
  --require-surface run:--capture-dir \
  --require-surface inspect:--json \
  --require-surface cancel:--run-id \
  --require-surface kill:--run-id
```

The report (one line, shown reformatted here):

```json
{
  "probe_version": 1,
  "binary": "processkit-cli",
  "version": "0.2.2",
  "schema_version": 1,
  "exit_code_band": { "start": 100, "end": 119 },
  "surface": ["cancel", "cancel:--run-id", "inspect", "..."],
  "compatible": true,
  "mismatches": []
}
```

- Pin `schema_version` (`--require-schema-version <N>`) and the reserved
  exit-code band (`--require-exit-code-band <start>-<end>`) so a future
  breaking change is caught here, before a run, rather than by a JSONL parser
  or an exit-code table drifting silently out of sync.
- Pin the exact CLI flags the adapter is about to use with one
  `--require-surface <token>` per token (a bare subcommand name, or
  `<subcommand>:--<long-flag>`) — this is how an adapter confirms a flag it
  depends on (for example `run:--capture-dir`) actually exists on this build
  before passing it.
- An unmet expectation makes `probe` exit **`PROBE_INCOMPATIBLE` (110)** with
  `compatible: false` and the concrete `mismatches`; a malformed `--require-*`
  argument (not an incompatibility, a bad flag) is the ordinary `USAGE` (100).
  A satisfied — or unrequested — surface exits `0`.

This is a **fail-closed** contract: an adapter that skips the preflight (or
silently proceeds after a `PROBE_INCOMPATIBLE`) re-introduces exactly the
uncontained-launch hazard this project exists to prevent. See
[`src/probe.rs`](../src/probe.rs) and the normative exit-code table in
[`docs/exit-codes.md`](exit-codes.md).

`probe --print-schema` is a separate, simpler mode on the same subcommand: it
prints this binary's embedded JSONL event-schema document instead of the
report above and exits `0`, so an adapter that only needs the schema for its
own version — no clone, no tag to match — can fetch it offline without a
compatibility check. It **cannot be combined with any `--require-*` flag**:
that combination is rejected as an ordinary `USAGE` (100) parse error, never a
silent skip of the requested checks, so it can never produce a false "ok" on
an invocation that also asked `probe` to verify expectations. See
[`docs/schema.md`](schema.md), "Getting the schema without a git checkout".

## 2. Launching a run

The recommended invocation for an adapter:

```sh
processkit-cli run \
  --run-id build-42 \
  --jsonl .processkit/build-42.jsonl \
  --capture-dir .processkit/build-42/capture \
  --env-clear \
  --env PATH="$PATH" \
  --env-remove CI_SECRET_TOKEN \
  --timeout 10m \
  --grace 5s \
  -- dotnet build
```

- **`--jsonl <file>`** is the only place lifecycle events are written — never
  stdout, so the child's own stdout/stderr stay pristine. Give every run a
  distinct path; the file is created or truncated at the start of the run.
- **`--run-id <id>`** is the identifier `inspect`/`cancel`/`kill` later match
  on — supply one you control (rather than the generated default) so the
  supervision step (§4) has a stable handle. Two live runs sharing one
  `--run-id` is legal but makes every supervision command against it fail
  closed as *ambiguous* (§4, §6) — keep run ids unique across an adapter's own
  concurrently-live runs.
- **`--capture-dir <dir>`** additionally tees stdout/stderr to
  `<dir>/stdout.log` / `<dir>/stderr.log` with a byte count, a SHA-256, and
  explicit truncation/write-error flags per stream (the `output_captured`
  event, §3) — use this when the adapter needs the transcript as a file rather
  than (or in addition to) the live echo.
- **`--no-echo`** suppresses the runner's own live retransmission of the
  child's stdout/stderr — the exact "pure noise" an adapter reading results
  from `--jsonl`/`--capture-dir` alone does not want interleaved with its own
  output. The pipe, `--capture-dir`, and the JSONL stream are all unaffected;
  it conflicts with `--inherit-stdio`, which runs no pump to suppress in the
  first place.
- **`--detach`** returns as soon as the run has provably started instead of
  blocking for its whole duration — the "launch and let go" shape, for an
  adapter that supervises out of band (§4) rather than by staying the runner's
  parent. It re-spawns the CLI detached (a new session on Unix, a
  `DETACHED_PROCESS` on Windows) and waits only until that copy has registered
  the run and written `run_started` to `--jsonl`, so on return the run is
  already visible to `list`/`inspect`/`wait`. An adapter that captures the
  launch command's output (`subprocess.run(..., capture_output=True)`) gets
  end-of-file when the call returns, not when the run ends: the detached runner
  keeps none of the caller's pipes open. **The exit code changes meaning
  under this flag and only under it**: it reports the *start* — `0` once the run
  started, or the same reserved code the failure would have produced in the
  foreground (a missing program is still `SPAWN` 101) — never the child's own
  code, which stays in the terminal `runner_exit` event (§3). Adapters that need
  the child's result must read it there, or via `wait` plus the event stream.
  It conflicts with `--inherit-stdio`/`--inherit-stdin` (nothing interactive
  survives detaching) and implies `--no-echo`'s discarding sinks, while
  `--jsonl`, `--capture-dir`, and `--idle-timeout` behave exactly as they do in
  the foreground. On Windows, pair it with `--create-no-window` for a console
  child: the detached runner has no console to lend it, so the OS gives the
  child one of its own. See [`docs/exit-codes.md`](exit-codes.md), "Detached
  runs".
- **`--env-clear` / `--env-remove <KEY>` / `--env <KEY=VALUE>`** give the
  adapter control over the child's environment, applied in that fixed order —
  clear, then remove, then set — regardless of flag order on the command line,
  so an explicit `--env` always wins on a duplicated key. See `README.md`,
  "Environment", for the full precedence rule.
- **`--max-memory <size>` / `--max-processes <n>` / `--cpu-quota <cores>`** cap
  the run's whole process tree. Enforcement needs a real container (Windows Job
  Object or Linux cgroup v2 at the real hierarchy root); where the platform or
  environment can't apply a cap the run fails **fast** with a `limit_hit` event
  (§3) and `BACKEND` (102) rather than running silently unbounded — so an adapter
  that *depends* on a cap must treat a `limit_hit` as a hard failure, not a
  warning. See `README.md`, "Resource limits", for the platform matrix (macOS/BSD
  and the Linux process-group fallback are unsupported; cgroup v2 is often
  unenforceable under systemd/containers/typical CI) and the Linux
  `--max-processes` caveat.
- **Command-line redaction.** `run_started`'s `command` field is redacted by
  default: the raw argv is *not* recorded, only a one-way SHA-256
  fingerprint (`argv_sha256`) and a classified worker-shape `hint` (both
  derived from argv but unable to reveal it) — filled on every run whether or
  not `--argv-raw` is given. Pass `--argv-raw` only when the adapter's own
  storage for the resulting JSONL is at least as trusted as the command line
  itself; do not default to it. See [`docs/schema.md`](schema.md#command-redaction)
  ("Command redaction") for the exact fingerprint encoding, which an adapter
  reproducing the digest independently must match byte for byte.

`run` is not shell-free by accident — everything after `--` is the literal
`<program> <args...>`, with no shell to expand or reinterpret it; an adapter
that needs shell features passes the shell as the program explicitly.

## 3. Reading the JSONL stream

`--jsonl` accumulates one JSON object per line as the run proceeds; parse it as
newline-delimited JSON, dispatching on each object's `event` field. A minimal
reader:

```python
import json

with open(jsonl_path, encoding="utf-8") as f:
    for line in f:
        evt = json.loads(line)
        if evt["schema_version"] != 1:
            raise IncompatibleSchema(evt["schema_version"])
        handle(evt["event"], evt)
```

Pin `schema_version` here too (or rely on the `probe` preflight in §1 to have
already ruled out a mismatch) — never assume a fixed shape without checking it.

**Ordering** (normative: [`docs/schema.md`](schema.md#ordering)). A normal run
emits, in order:

1. `run_started` — the child was spawned; carries `run_id`, `root_pid`,
   containment `mechanism`, the `abrupt_cleanup` tri-state, and the redacted
   `command`.
2. `members_snapshot` — the container's members at that point.
3. Either the natural-exit path (`root_exited`, `cleanup_started`,
   `cleanup_finished`) or a runner-imposed ending's reason event (`timeout`,
   `cancelled`, or `killed`) followed by the same `cleanup_started` /
   `cleanup_finished` pair.
4. `output_captured`, only when `--capture-dir` was set.
5. `runner_exit` — always the **last line**, the terminal event of every run,
   including a runner failure before the child ever started (in which case
   `spawn_failed` or `container_failed` precedes it instead, with no
   `run_started`).

**Telling outcomes apart.** Two signals distinguish how a run ended, and an
adapter should use both together: the process's own **exit code** (fastest to
check, no parsing needed) and the terminal `runner_exit` event's `source` and
`code` fields (authoritative — see [`docs/exit-codes.md`](exit-codes.md#why-a-band-is-not-enough-on-its-own),
"Why a band is not enough on its own"):

| `runner_exit.source` | Exit code | Meaning |
| --- | --- | --- |
| `child_exit` | the child's own code (`child_code`, echoed in `code` too) | The child ran to completion on its own. |
| `timeout` | `106` | A runner deadline elapsed and the runner tore the tree down — the whole-run `--timeout` or the `--idle-timeout` (child silent past the idle window). The preceding `timeout` event's `reason` (`overall` / `idle`) says which; both reuse this one source and code. |
| `cancelled` | `107` | A local stop signal cancelled the run: a `Ctrl-C`, on Unix a `SIGTERM`/`SIGHUP` (an external `kill`/`systemctl stop`/cancelled CI job, or a hung-up terminal), or on Windows a `Ctrl-Break`/console close/logoff/system shutdown. The preceding `cancelled` event's `source` (`ctrl_c` / `sigterm` / `sighup` / `ctrl_break` / `ctrl_close` / `ctrl_logoff` / `ctrl_shutdown`) says which; all reuse this one source and code. |
| `control_cancel` | `108` | A control-plane `cancel` (§4) cancelled the run. |
| `control_kill` | `109` | A control-plane `kill` (§4) force-killed the run. |
| `spawn_error` | `101` | The child never started (`spawn_failed` precedes it). |
| `container_error` | `102` | The container could not be created or joined (`container_failed` precedes it) — including a requested resource limit (`--max-memory`/`--max-processes`/`--cpu-quota`) the platform could not apply, in which case a `limit_hit` naming the limit precedes the `container_failed` (see [`docs/schema.md`](schema.md#limit_hit)). |
| `internal` | `104` | A genuine runner bug — the runner's own logic hit a state it rules out. |
| `setup` | `111` | An ordinary fail-closed setup failure (an unwritable `--jsonl`/`--capture-dir`, an unreadable `--stdin-file`) — distinct from `internal`, and the caller can usually act on it (bad path, permissions, resources). |

Only `source: "child_exit"` carries a non-null `child_code`; every other
source means the child's own exit code was never produced or is not what
`code` reports, and `child_code` is `null`. See the full field reference in
[`docs/schema.md`](schema.md#runner_exit) and the exit-code contract in
[`docs/exit-codes.md`](exit-codes.md).

## 4. Supervising a live run: `inspect` / `cancel` / `kill` / `wait`

Once a run has started (its `run_id` is known — supplied at launch, per §2),
an adapter can query, steer, and wait for it while it is still live. Every
command resolves the target purely by `run_id` through the per-user registry —
never by PID. This is also the whole supervision story for a run launched with
`--detach` (§2): a detached run is an ordinary run in the registry, and these
four commands are how an adapter that is no longer its parent watches and steers
it:

```sh
processkit-cli inspect --run-id build-42 --json
processkit-cli cancel  --run-id build-42
processkit-cli kill    --run-id build-42
processkit-cli wait    --run-id build-42 --timeout 10m
```

The first three reach the live runner over the local control plane described
normatively in [`docs/control-plane.md`](control-plane.md); `wait` does not
contact the runner at all and is described in [`docs/registry.md`](registry.md),
"Waiting — `wait`".

- **`inspect`** is read-only: it prints a JSON snapshot (`mechanism`,
  `root_pid`, `started_at`, the current `members`) to stdout and changes
  nothing.
- **`cancel`** ends the run through the *same* soft-stop → grace → hard-kill
  teardown a `--timeout` or a local `Ctrl-C` drives, exiting the run with
  `CONTROL_CANCELLED` (`108`).
- **`kill`** hard-kills the whole tree **immediately** — no soft stop, no
  grace — exiting the run with `CONTROL_KILLED` (`109`).
- **`wait`** blocks until the run is no longer live and exits `0`. It is the
  answer for an adapter that is **not** the runner's parent — one that
  restarted, or that supervises runs another process launched — and so has no
  child process to wait on. It prints nothing (the exit code is the answer),
  never touches the run, and needs no control endpoint, so it also works for a
  run whose transport never came up.

Both mutating verbs' outcomes are also written to the *target run's own*
`--jsonl` stream (a `cancelled`/`killed` event with `source`
`control_cancel`/`control_kill`, and the matching terminal `runner_exit`), so
an adapter watching that stream sees the command take effect even without
reading the `cancel`/`kill` client's own ack.

**Waiting for a run an adapter did not launch.** The typical shape — cancel a
run, then confirm it is really gone before releasing the resources it held:

```sh
processkit-cli cancel --run-id build-42          # 0: the runner acked
processkit-cli wait   --run-id build-42 --timeout 30s
case $? in
  0)   ;;                    # the run is over; its own exit/JSONL say how it ended
  112) ;;                    # still live at the deadline — the run was NOT touched
  103) ;;                    # ambiguous run id: more than one live run uses it (§6)
esac
```

- **`0`** means "not running". It is also what an unknown `run_id` returns, on
  purpose: a clean exit deletes its own registry entry, so "never registered"
  and "already finished and cleaned up" are the same observation, and failing
  on the second would turn the ordinary "it finished while I was starting up"
  race into an error. The flip side an adapter must respect: a typo'd
  `run_id` also returns `0`, so **never read `wait`'s `0` as proof the run
  existed** — establish that from the launch itself or from `list` (§5).
- **`WAIT_TIMEOUT` (112)** is *the waiter's* deadline, not the run's: the run
  was left running and untouched, and is still going. Do not confuse it with
  the run's own `TIMEOUT` (`106`) in §3's table, which means the runner tore
  the tree down. Retrying the same `wait` is a reasonable response to a `112`.
- **`CONTROL` (103)** here means only one thing — an ambiguous `run_id` (§6);
  `wait` has no runner to fail to reach.
- Without `--timeout`, `wait` blocks indefinitely. Prefer an explicit deadline
  in an adapter, so a supervisor never inherits an unbounded wait.

**`CONTROL` (103)** is the one exit code all four of these clients can return,
for the same underlying reason: the command could not be resolved to *the*
single target run. See §6 for the concrete situations that produce it.

## 5. Housekeeping: `list` / `prune`

`list` and `prune` scan the registry directly rather than reaching a specific
live run, and are the tools for an adapter that manages many runs or wants to
clean up after abrupt failures — see the normative "Discovery" and "Reaping"
sections of [`docs/registry.md`](registry.md).

```sh
processkit-cli list  --json   # every registered run, whatever its health
processkit-cli prune --json   # reap only the confirmed-stale entries
```

- **`list --json`** prints one JSON object per registry entry (`run_id`,
  health, `started_at`, `hint`, `argv_sha256`, `endpoint`), sorted
  deterministically. `argv_sha256` and `hint` are the same redaction-safe
  command identification the `run_started` event carries (§3) — the full
  64-character digest here, so an adapter can join a registry entry to the
  events of the run that wrote it, or group several live entries by "same
  command" without ever handling a command line. Both are `null` on a record
  written before those fields existed, and `hint` is `null` for the common
  case of a command matching no known worker shape. Health is
  `live`, `stale` (**confirmed** dead — no live holder found), or `unprobed`
  (the liveness lock could not even be opened, e.g. permission denied — a
  distinct, additive value: liveness is *unknown*, never printed as the
  confirmed-dead `stale`). All three are listed, never hidden — a stale entry
  (a leftover from a runner that died abruptly) is exactly what an operator or
  adapter wants visible here, and an unprobed one is exactly the case where
  guessing would mislead.
- **`prune --json`** deletes only entries it can *confirm* are stale, printing
  a tally: `{"pruned":N,"live":N,"unprobed":N,"orphaned_locks":N}`. A live run
  is never touched, and an entry whose liveness could not even be probed is
  left in place rather than guessed at — see "The reaping safety invariant" in
  [`docs/registry.md`](registry.md#the-reaping-safety-invariant). On unix each
  reaped entry also takes with it the private control-socket directory that
  record published, so an abruptly-killed run leaves no `pkc-…` litter in the
  temp directory either; the tally fields are unchanged (that socket is counted
  by its own entry's `pruned`). Worth scheduling if your adapter starts many
  runs — see "Reaping the control socket" in
  [`docs/registry.md`](registry.md#reaping-the-control-socket).

Both are read-only with respect to any *live* run's control transport; neither
carries the "could not reach the target run" failure modes of §4.

## 6. Typical errors

- **Stale registry entry.** The runner behind a `run_id` died abruptly
  (crash, `SIGKILL`, a parent's Job Object terminate); its record is left
  behind but its liveness lock is released. `inspect`/`cancel`/`kill` detect
  this *before* connecting and report it as a `CONTROL` (103) failure with an
  explanatory message on stderr — never a hang, and never silently treated as
  live. `list` still shows the entry (marked `stale`); `prune` is what removes
  it. An ordinary Unix `SIGTERM`/`SIGHUP`, or a Windows `Ctrl-Break`/console
  close/logoff/system shutdown, is **not** in this class: the runner catches
  those signals/events and runs the full cancel teardown (a `cancelled` event,
  the cleanup pair, `runner_exit` `cancelled`/`107`, and removal of the registry
  entry), so stopping a run with `kill <pid>` (Unix) or a closed console
  (Windows) leaves neither a stale entry nor a surviving descendant.
- **Unprobeable registry entry.** The entry's liveness lock could not be probed
  at all (permission denied, a rejected symlink/reparse point, a non-regular
  file in its place), so nothing about the run is confirmed either way. This is
  the same `CONTROL` (103) refusal — `inspect`/`cancel`/`kill` act only on a
  **confirmed-live** entry — but it is reported honestly as `unprobed`, not as
  a gone runner; `list` shows the same entry as `unprobed` and `prune` leaves
  it in place. Investigate the registry directory rather than deleting the
  record by hand (see [`docs/troubleshooting.md`](troubleshooting.md)).
- **Died mid-conversation.** The registry entry read as live, but the runner
  exited between the liveness check and the reply reaching the client — the
  connect fails, or the connection closes before a complete response. Also a
  bounded `CONTROL` (103) failure, never a wedge: every wait in the control
  plane (connecting, and the request/response exchange) is deadline-bounded.
- **Ambiguous `run_id`.** The registry does not enforce `run_id` uniqueness;
  if more than one **live** entry matches, every by-`run-id` command — the
  read-only `inspect` and `wait` included — fails closed with `CONTROL` (103)
  rather than guessing which entry the scan happened to return first. Keep
  `run_id`s unique among an adapter's own concurrently-live runs (§2) to avoid
  this entirely.
- **`CONTROL`-class exit codes are not run outcomes.** A `103` from
  `inspect`/`cancel`/`kill`/`wait` describes the *client's* inability to
  resolve or reach a single target — it says nothing about how the target run
  itself ended (or is still running). Do not conflate it with the run-outcome
  codes in §3's table (`106`–`109`, or the child's own code); those come only
  from the run's own process exit and its `runner_exit` event. The same
  separation applies to `WAIT_TIMEOUT` (112): it is the *waiting client*
  giving up, never the run being stopped (§4).
- **A `--detach` exit code is not a run outcome either.** `run --detach`'s `0`
  means "the run started", not "the child succeeded", and its non-zero codes
  mean "the run never started" — carrying the same reserved code the failure
  would have produced in the foreground. An adapter that branches on a detached
  launch's exit code as if it were the child's result will read every
  long-running failure as a success; the child's outcome is in the terminal
  `runner_exit` event (§3), reached after `wait` (§4). See
  [`docs/exit-codes.md`](exit-codes.md#detached-runs-the-code-reports-the-start),
  "Detached runs".
- **`SETUP` (111) vs. `INTERNAL` (104).** A `run` that could not write its
  `--jsonl`/`--capture-dir`, or open a `--stdin-file`, fails closed with
  `SETUP` (111) — an ordinary, usually-actionable environment problem (bad
  path, permissions), not a runner bug. `INTERNAL` (104) is reserved for a
  genuine invariant violation in the runner's own logic. See "Setup failures
  vs internal faults" in [`docs/exit-codes.md`](exit-codes.md#setup-failures-vs-internal-faults).

## See also

- [`docs/schema.md`](schema.md) — the normative JSONL event schema (every
  field, every event, versioning rules).
- [`docs/exit-codes.md`](exit-codes.md) — the normative reserved exit-code
  band and the child-fidelity rule.
- [`docs/control-plane.md`](control-plane.md) — the normative local transport,
  wire protocol, and `inspect`/`cancel`/`kill` behavior.
- [`docs/registry.md`](registry.md) — the normative registry location, record
  format, and staleness/reaping rules.
- [`docs/architecture.md`](architecture.md) — the map of this repository's own
  modules, for a contributor rather than a consumer.
- [`docs/troubleshooting.md`](troubleshooting.md) — symptom-to-cause diagnosis
  for an operator, organized by what you observe rather than by call
  sequence.
