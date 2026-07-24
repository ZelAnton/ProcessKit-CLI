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
- **`--env-clear` / `--env-remove <KEY>` / `--env <KEY=VALUE>`** give the
  adapter control over the child's environment, applied in that fixed order —
  clear, then remove, then set — regardless of flag order on the command line,
  so an explicit `--env` always wins on a duplicated key. See `README.md`,
  "Environment", for the full precedence rule.
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
| `timeout` | `106` | `--timeout` elapsed; the runner tore the tree down. |
| `cancelled` | `107` | A local `Ctrl-C` cancelled the run. |
| `control_cancel` | `108` | A control-plane `cancel` (§4) cancelled the run. |
| `control_kill` | `109` | A control-plane `kill` (§4) force-killed the run. |
| `spawn_error` | `101` | The child never started (`spawn_failed` precedes it). |
| `container_error` | `102` | The container could not be created or joined (`container_failed` precedes it). |
| `internal` | `104` | A genuine runner bug — the runner's own logic hit a state it rules out. |
| `setup` | `111` | An ordinary fail-closed setup failure (an unwritable `--jsonl`/`--capture-dir`, an unreadable `--stdin-file`) — distinct from `internal`, and the caller can usually act on it (bad path, permissions, resources). |

Only `source: "child_exit"` carries a non-null `child_code`; every other
source means the child's own exit code was never produced or is not what
`code` reports, and `child_code` is `null`. See the full field reference in
[`docs/schema.md`](schema.md#runner_exit) and the exit-code contract in
[`docs/exit-codes.md`](exit-codes.md).

## 4. Supervising a live run: `inspect` / `cancel` / `kill`

Once a run has started (its `run_id` is known — supplied at launch, per §2),
an adapter can query and steer it while it is still live, over the local
control plane described normatively in
[`docs/control-plane.md`](control-plane.md). Every verb resolves the target
purely by `run_id` through the per-user registry — never by PID:

```sh
processkit-cli inspect --run-id build-42 --json
processkit-cli cancel  --run-id build-42
processkit-cli kill    --run-id build-42
```

- **`inspect`** is read-only: it prints a JSON snapshot (`mechanism`,
  `root_pid`, `started_at`, the current `members`) to stdout and changes
  nothing.
- **`cancel`** ends the run through the *same* soft-stop → grace → hard-kill
  teardown a `--timeout` or a local `Ctrl-C` drives, exiting the run with
  `CONTROL_CANCELLED` (`108`).
- **`kill`** hard-kills the whole tree **immediately** — no soft stop, no
  grace — exiting the run with `CONTROL_KILLED` (`109`).

Both mutating verbs' outcomes are also written to the *target run's own*
`--jsonl` stream (a `cancelled`/`killed` event with `source`
`control_cancel`/`control_kill`, and the matching terminal `runner_exit`), so
an adapter watching that stream sees the command take effect even without
reading the `cancel`/`kill` client's own ack.

**`CONTROL` (103)** is the one exit code every one of these three clients can
return, for the same reason in every case: the target run could not be
reached. See §6 for the concrete situations that produce it.

## 5. Housekeeping: `list` / `prune`

`list` and `prune` scan the registry directly rather than reaching a specific
live run, and are the tools for an adapter that manages many runs or wants to
clean up after abrupt failures — see the normative "Discovery" and "Reaping"
sections of [`docs/registry.md`](registry.md).

```sh
processkit-cli list  --json   # every registered run, live and stale
processkit-cli prune --json   # reap only the confirmed-stale entries
```

- **`list --json`** prints one JSON object per registry entry (`run_id`,
  `live`/`stale` health, `started_at`, `endpoint`), sorted deterministically.
  Both live and stale entries are listed — a stale entry (a leftover from a
  runner that died abruptly) is exactly what an operator or adapter wants
  visible here, not hidden.
- **`prune --json`** deletes only entries it can *confirm* are stale, printing
  a tally: `{"pruned":N,"live":N,"unprobed":N}`. A live run is never touched,
  and an entry whose liveness could not even be probed is left in place rather
  than guessed at — see "The reaping safety invariant" in
  [`docs/registry.md`](registry.md#the-reaping-safety-invariant).

Both are read-only with respect to any *live* run's control transport; neither
carries the "could not reach the target run" failure modes of §4.

## 6. Typical errors

- **Stale registry entry.** The runner behind a `run_id` died abruptly
  (crash, `SIGKILL`, a parent's Job Object terminate); its record is left
  behind but its liveness lock is released. `inspect`/`cancel`/`kill` detect
  this *before* connecting and report it as a `CONTROL` (103) failure with an
  explanatory message on stderr — never a hang, and never silently treated as
  live. `list` still shows the entry (marked `stale`); `prune` is what removes
  it.
- **Died mid-conversation.** The registry entry read as live, but the runner
  exited between the liveness check and the reply reaching the client — the
  connect fails, or the connection closes before a complete response. Also a
  bounded `CONTROL` (103) failure, never a wedge: every wait in the control
  plane (connecting, and the request/response exchange) is deadline-bounded.
- **Ambiguous `run_id`.** The registry does not enforce `run_id` uniqueness;
  if more than one **live** entry matches, every verb — including read-only
  `inspect` — fails closed with `CONTROL` (103) rather than guessing which
  entry the scan happened to return first. Keep `run_id`s unique among an
  adapter's own concurrently-live runs (§2) to avoid this entirely.
- **`CONTROL`-class exit codes are not run outcomes.** A `103` from
  `inspect`/`cancel`/`kill` describes the *client's* inability to reach a
  target — it says nothing about how the target run itself ended (or is
  still running). Do not conflate it with the run-outcome codes in §3's table
  (`106`–`109`, or the child's own code); those come only from the run's own
  process exit and its `runner_exit` event.
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
