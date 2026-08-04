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

## 1. Fail-closed preflight: `probe` (the binary), then `doctor` (the host)

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
- Pin **`attest:peer-identity`** if the adapter will gate anything on
  containment membership (§4). It is the one token in `surface` that is a
  *capability* rather than a spelling — note the missing `--` — and it says this
  build can obtain a kernel-authenticated identity for a control-plane client on
  this platform, which is what makes `attest` able to answer at all. Requiring it
  turns "this platform cannot prove membership" into an ordinary fail-closed
  `PROBE_INCOMPATIBLE` (110) here, instead of a `peer_identity_unsupported`
  refusal in the middle of a job. Its presence is a guarantee; its absence
  withholds one rather than predicting failure, so an adapter that requires it is
  choosing not to depend on an unguaranteed capability.
- An unmet expectation makes `probe` exit **`PROBE_INCOMPATIBLE` (110)** with
  `compatible: false` and the concrete `mismatches`; a malformed `--require-*`
  argument (not an incompatibility, a bad flag) is the ordinary `USAGE` (100).
  A satisfied — or unrequested — surface exits `0`.

This is a **fail-closed** contract: an adapter that skips the preflight (or
silently proceeds after a `PROBE_INCOMPATIBLE`) re-introduces exactly the
uncontained-launch hazard this project exists to prevent. See
[`src/probe.rs`](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/src/probe.rs) and the normative exit-code table in
[`docs/exit-codes.md`](exit-codes.md).

The report's shape is published as a JSON Schema with a golden fixture —
`fixtures/schema/cli/probe.schema.json` and `probe.jsonl` — so an adapter can
validate what it parsed instead of re-deriving the shape by hand. Every
machine-readable output in this guide has such a pair; see
[`fixtures/schema/cli/README.md`](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/fixtures/schema/cli/README.md)
for the full table, and `docs/compatibility.md`, "Machine-output schemas", for
why some of these outputs carry no version field of their own (the `probe`
report's `probe_version`, the `inspect` snapshot's `snapshot_version`, the
failure envelope's `error_version` — §7 — the attestation's
`attestation_version` — §4 — and the qualification report's `doctor_version`
— below — are the five that do).

`probe --print-schema` is a separate, simpler mode on the same subcommand: it
prints this binary's embedded JSONL event-schema document instead of the
report above and exits `0`, so an adapter that only needs the schema for its
own version — no clone, no tag to match — can fetch it offline without a
compatibility check. It **cannot be combined with any `--require-*` flag**:
that combination is rejected as an ordinary `USAGE` (100) parse error, never a
silent skip of the requested checks, so it can never produce a false "ok" on
an invocation that also asked `probe` to verify expectations. See
[`docs/schema.md`](schema.md), "Getting the schema without a git checkout".

### Qualifying the host: `doctor`

A passing `probe` says the binary you found is the one you need. It does not say this
*machine* can run a contained process — by construction, since proving that would mean
running one, and a preflight that spawned a child would not be a preflight. The two
claims come apart in practice: a registry directory that cannot be created or is not
owner-only, a containment mechanism the kernel will not hand out, a local IPC endpoint
that will not bind. Each of those passes every `--require-*` check above and fails the
first production run.

`doctor` closes that gap by doing the thing:

```sh
processkit-cli doctor --json   --require-abrupt-cleanup whole_tree   --check-resource-controller --require-resource-controller
```

It performs a bounded scratch run of this binary's own harmless child
(`doctor --scratch-child`), drives that run through the ordinary control plane
(`inspect`, `cancel`, terminal wait), confirms teardown left nothing, and reports the
facts it observed — the registry directory and its owner-only protection, the
containment mechanism and abrupt-cleanup level this host really gives a run, the
transport round-trip, a confirmed-empty cleanup, and per-phase timings. On success it
leaves nothing behind; on a failed phase it keeps a diagnostics directory and names it
in the report (`diagnostics_dir`).

Where it belongs in an adapter's flow: **once per host, at setup or install time**, not
before every run. It is the counterpart of the `probe` above, on the other axis —

| | `probe` | `doctor` |
| --- | --- | --- |
| Subject | This binary | This host |
| Side effects | None: no child, no registry, no container, no endpoint | A real scratch run: registry entry, container, control endpoint, all cleaned up |
| Cost | Milliseconds | Under a second, bounded by `--timeout` (default `30s`) |
| Run it | Before every launch, or at least whenever the binary may have changed | Once per host, at setup time — or when a host starts behaving differently |
| Fail-closed code | `PROBE_INCOMPATIBLE` (110) | `HOST_UNQUALIFIED` (116) |

The requirement flags gate the **exit code** only; the report carries the observed
facts either way, so an adapter can act on the code and still log everything the
qualification saw. `--require-resource-controller` needs
`--check-resource-controller` alongside it — a requirement about a fact this
invocation never observed is refused as a `USAGE` (100) rather than guessed.

`doctor --json`'s shape is published like every other machine-readable output here:
`fixtures/schema/cli/doctor.schema.json` and `doctor.jsonl`. See
[`docs/troubleshooting.md`](troubleshooting.md), "Qualifying a host: `doctor`", for
reading a negative verdict, and [`docs/exit-codes.md`](exit-codes.md), "Qualifying a
host: `doctor`", for why `116` is its own code.

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
- **`--run-id <id>`** is the identifier `inspect`/`cancel`/`kill`/`attest` later
  match on — supply one you control (rather than the generated default) so the
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
  `--jsonl`, `--capture-dir`, `--idle-timeout`, and `--snapshot-interval` behave
  exactly as they do in the foreground. The detached runner's own stderr is
  `null`, so `--jsonl` is the only channel that reports anything — including a
  failed member read, which is why that failure is a flagged event rather than a
  warning (§3). On Windows, pair it with `--create-no-window` for a console
  child: the detached runner has no console to lend it, so the OS gives the
  child one of its own. See [`docs/exit-codes.md`](exit-codes.md), "Detached
  runs".
- **`--env-clear` / `--env-remove <KEY>` / `--env-file <file>` / `--env
  <KEY=VALUE>`** give the
  adapter control over the child's environment, applied in that fixed order —
  clear, then remove, then files, then explicit sets — regardless of flag order on
  the command line, so an explicit `--env` always wins on a duplicated key. See
  `README.md`, "Environment", for the full precedence rule.
- **`--run-id-env <KEY>`** hands the child the run's *final* id — the `--run-id`
  above, or the generated one when the adapter did not supply an id — in the named
  environment variable, applied after every flag in the previous bullet. This is
  the alternative to minting an identity adapter-side and passing it twice
  (`--run-id <id> --env KEY=<id>`): one value, no second copy to drift, and it is
  the *only* way to give the child a runner-generated id, which is otherwise not
  knowable until the run has already started. It is opt-in (no key is injected by
  default) and it composes with `--detach` — the detached copy is re-spawned with
  an explicit `--run-id` for the id its caller already reported, so the child sees
  the same value the caller has. An explicit `--env <KEY>=…` for the same key is
  refused as a `USAGE` (100) parse error rather than silently overridden — "the
  same key" by the platform's own rule, so on Windows an `--env` entry differing
  from `<KEY>` only in case is that same refusal. Treat the
  value as **correlation data only**: it identifies a run, it does not
  authenticate one, and any process able to set an environment variable can forge
  it.
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

**Or let the binary read it back for you.** `events` is the first-party reader of
the same stream, so an adapter that only needs to *show* a run's story — or to
check one — does not have to write the loop above at all:

```sh
processkit-cli events --run-id build-42               # rendered for a human
processkit-cli events --run-id build-42 --follow      # ... as it happens
processkit-cli events --file "$jsonl" --json          # the runner's own bytes
processkit-cli events --file "$jsonl" --validate      # conformance check
```

It resolves the stream through the registry (`--run-id`, the same `jsonl` locator
`list --json` publishes in §5) or reads a path directly (`--file`, for a stream
whose registry record is already gone — a clean exit deletes its own record — or
one this registry never knew about). Exactly one of the two is required and they
are mutually exclusive; there is no precedence rule, so passing both is a `USAGE`
(100) error. Like `list`/`wait` it is read-only: registry opened read-only, no
control-plane round trip, nothing mutated.

Three properties matter for an adapter:

- **`--json` is a pass-through, not a re-serialization.** Each line is emitted
  byte for byte as the runner wrote it, so a field a *newer* runner added survives
  the trip — the pipeline `processkit-cli events --file … --json | your-parser` is
  exactly as lossless as reading the file yourself. A line that is not JSON is
  reported on stderr instead of emitted, so stdout stays parseable JSONL.
- **`--follow` is bounded by the run, never by an invented deadline.** It returns
  at the terminal `runner_exit`, or once the registry reports the run over and the
  stream has stopped growing — the abrupt-death case, explained on stderr rather
  than passed off as a complete stream. It hands out only *complete* lines, so a
  half-written event is never parsed as an event.
- **`--validate` is a conformance gate.** It checks every line against the schema
  document this binary embeds — the same one `probe --print-schema` prints in §1 —
  reports each violation by line number and by what it violated, and exits
  `EVENTS_INVALID` (114) if any line fails, `0` if none does. An unreadable stream
  is still `SETUP` (111) and a `--run-id` naming no single stream is still
  `CONTROL` (103), so a fixture-checking CI job can tell "invalid" from "could not
  be checked". This is the recommended way for an adapter to keep its own recorded
  fixtures honest against the runner version it targets.

**Ordering** (normative: [`docs/schema.md`](schema.md#ordering)). A normal run
emits, in order:

1. `run_started` — the child was spawned; carries `run_id`, `root_pid`,
   containment `mechanism`, the `abrupt_cleanup` tri-state, and the redacted
   `command`.
2. `members_snapshot` (`reason: "spawn"`) — the container's members at that
   point. Exactly one by default; a run started with `--snapshot-interval
   <duration>` emits **additional** `members_snapshot` events (`reason:
   "interval"`) on that cadence, all of them after this one and all of them
   before step 3 — never inside the teardown pair. Route by event type and treat
   the count as open-ended: within a schema version an adapter must not assume an
   event type it knows occurs only once (the full list of what a reader must
   tolerate within a version is in
   [`docs/compatibility.md`](compatibility.md#what-a-reader-must-tolerate-within-one-version)).
   Every one of these events carries `read_error`; when it is `true` the read
   failed and `members` is an empty fallback, not a confirmed-empty tree — check
   the flag before drawing a conclusion about the tree from an empty array.
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

## 4. Supervising a live run: `inspect` / `cancel` / `kill` / `attest` / `wait`

Once a run has started (its `run_id` is known — supplied at launch, per §2),
an adapter can query, steer, and wait for it while it is still live. Every
command resolves the target purely by `run_id` through the per-user registry —
never by PID. This is also the whole supervision story for a run launched with
`--detach` (§2): a detached run is an ordinary run in the registry, and these
five commands are how an adapter that is no longer its parent steers it —
alongside `events` (§3), which reads that run's stream back without contacting it
at all:

```sh
processkit-cli inspect --run-id build-42 --json
processkit-cli cancel  --run-id build-42
processkit-cli kill    --run-id build-42
processkit-cli attest  --run-id build-42 --json
processkit-cli wait    --run-id build-42 --timeout 10m
```

The first four reach the live runner over the local control plane described
normatively in [`docs/control-plane.md`](control-plane.md); `wait` does not
contact the runner at all and is described in [`docs/registry.md`](registry.md),
"Waiting — `wait`".

- **`inspect`** is read-only: it prints a snapshot (`mechanism`, `root_pid`,
  `started_at`, the current `members`) to stdout and changes nothing — as
  JSON with `--json` (shown above), or a human-readable rendering by
  default.
- **`cancel`** ends the run through the *same* soft-stop → grace → hard-kill
  teardown a `--timeout` or a local `Ctrl-C` drives, exiting the run with
  `CONTROL_CANCELLED` (`108`).
- **`kill`** hard-kills the whole tree **immediately** — no soft stop, no
  grace — exiting the run with `CONTROL_KILLED` (`109`).
- **`attest`** answers one question about the *calling process*: is it inside
  this run's container? The runner decides it from the kernel's own record of who
  opened the control connection — there is no `--pid` and no way to ask about any
  other process — so a `member` answer is a containment fact rather than a string
  the caller carried. This is what turns an adapter's "the caller belongs to run
  X" convention into a runner-checked invariant: `verdict` `member` exits `0`,
  `not_a_member` exits `NOT_A_MEMBER` (115) — a *decided* answer, deliberately not
  a `CONTROL` (103) — and `peer_identity_unsupported` exits `103`, the fail-closed
  refusal a platform that cannot name its callers gives instead of an unproven
  "ok" (pin `attest:peer-identity` at preflight, §1, to rule that out up front).
  The attestation is printed on stdout for every verdict, including the failing
  ones, and carries its own `attestation_version` (§1) — this answer's contract
  axis, independent of `inspect`'s `snapshot_version`. The client reads it
  **strictly**: a reply declaring any number other than the single one this build
  implements is refused with `CONTROL` (103) and `kind: "incompatible_contract"`
  (§6) instead of being read as a verdict its sender never promised, because unlike
  a snapshot this answer is one an adapter gates access on — there is deliberately
  no read-down range. Read `mechanism` if you need to know how strong the
  containment behind a `member` answer is; nested runs, the per-mechanism scope, and
  why this axis has no read-down range are covered in
  [`docs/control-plane.md`](control-plane.md), "`attest`" and "Attestation version".
- **`wait`** blocks until the run is no longer live and exits `0`. It is the
  answer for an adapter that is **not** the runner's parent — one that
  restarted, or that supervises runs another process launched — and so has no
  child process to wait on. It prints nothing (the exit code is the answer),
  never touches the run, and needs no control endpoint, so it also works for a
  run whose transport never came up. Adding **`--report-outcome`** makes the
  single-run form print one JSON object, while the `--all` form prints one JSON
  array in stable snapshot order, naming how each run ended — `status` `reported`
  with the terminal event's `code`/`source`/`child_code`, or `status` `unknown`
  with all three `null` when the outcome could not be established — without
  changing any of the exit codes below. See
  [`docs/registry.md`](registry.md), "Waiting — `wait`".

Each of these outputs has a published JSON Schema and golden fixture under
`fixtures/schema/cli/`: `inspect.schema.json` (the single snapshot and the
`--all` array), `control-ack.schema.json` (the `cancel`/`kill` ack and the
`--all` report array), `attest.schema.json` (the attestation), and
`wait.schema.json` (`--report-outcome` in either single-run or aggregate form).

Both mutating verbs' outcomes are also written to the *target run's own*
`--jsonl` stream (a `cancelled`/`killed` event with `source`
`control_cancel`/`control_kill`, and the matching terminal `runner_exit`), so
an adapter watching that stream sees the command take effect even without
reading the `cancel`/`kill` client's own ack.

**Tearing down everything at once: `--all` (T-217).** `cancel --all` / `kill
--all` (mutually exclusive with `--run-id`, one of the two required) are the
aggregate counterpart to the by-`run_id` form above: instead of one named run
they act on every run confirmed live in a snapshot taken when the invocation
starts, applying the identical per-run mutation to each, and print a single
JSON array on stdout — one `{"run_id":...,"accepted":...}` entry per snapshot
target — instead of one ack. An adapter driving a full environment teardown
(e.g. before shutting down its own process) typically issues `cancel --all`
in place of a loop over individually-known `run_id`s, then `wait --all` /
`list --json` / `prune` to confirm the fleet is actually gone. See
[`docs/control-plane.md`](control-plane.md), "`cancel --all` / `kill --all`",
for the exit-code and report contract — it differs from the by-`run_id` form's
`103` in the note right below.

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

**`CONTROL` (103)** is the one exit code all five of these clients' by-`run_id`
form can return, and for all five the usual reason is the same: the command could
not be resolved to *the* single target run. Two further reasons belong to the
read-only verbs, where the target *was* resolved and reached and did answer. The
first is shared by both of them: a reply declaring a contract version this client
does not read is refused rather than acted on — `inspect`'s `snapshot_version` (see
`docs/control-plane.md`, "Snapshot version: a newer runner's reply is refused, an
older one is read") or `attest`'s `attestation_version` (ibid., "Attestation
version"). The second is `attest`'s alone: it was answered
`peer_identity_unsupported` — the runner could not name the caller, so it declined
to decide. `attest`'s *decided* negative is not a `103` at all but `NOT_A_MEMBER`
(115). See §6 for the concrete situations that produce a `103`, those included.
`cancel --all` / `kill --all`
reuse the same code for a different reason — one or more snapshot targets failed,
not "no single target run" — see the `--all` paragraph above and
`docs/control-plane.md`.

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

Their machine-readable shapes are published too: `fixtures/schema/cli/list.schema.json`
(one entry object per line) and `fixtures/schema/cli/prune.schema.json` (the plain
tally and, as `#/$defs/dryRunReport`, the `--dry-run` form with its `candidates`
list), each with a golden `*.jsonl` fixture beside it.

## 6. Typical errors

Every distinction in this section is available as a **machine-readable value**, not
only as prose: run any of these commands with the global `--error-format json` and
the failure prints one bounded JSON object on stderr whose `kind` names exactly the
case below (`stale`, `unprobed`, `ambiguous_run_id`, `incompatible_contract`, …).
The `kind` column is noted per bullet; §7 has the full contract.

- **Stale registry entry.** (`kind: "stale"`) The runner behind a `run_id` died abruptly
  (crash, `SIGKILL`, a parent's Job Object terminate); its record is left
  behind but its liveness lock is released. `inspect`/`cancel`/`kill`/`attest`
  detect this *before* connecting and report it as a `CONTROL` (103) failure with an
  explanatory message on stderr — never a hang, and never silently treated as
  live. `list` still shows the entry (marked `stale`); `prune` is what removes
  it. An ordinary Unix `SIGTERM`/`SIGHUP`, or a Windows `Ctrl-Break`/console
  close/logoff/system shutdown, is **not** in this class: the runner catches
  those signals/events and runs the full cancel teardown (a `cancelled` event,
  the cleanup pair, `runner_exit` `cancelled`/`107`, and removal of the registry
  entry), so stopping a run with `kill <pid>` (Unix) or a closed console
  (Windows) leaves neither a stale entry nor a surviving descendant.
- **Unprobeable registry entry.** (`kind: "unprobed"`) The entry's liveness lock could not be probed
  at all (permission denied, a rejected symlink/reparse point, a non-regular
  file in its place), so nothing about the run is confirmed either way. This is
  the same `CONTROL` (103) refusal — `inspect`/`cancel`/`kill`/`attest` act only on
  a **confirmed-live** entry — but it is reported honestly as `unprobed`, not as
  a gone runner; `list` shows the same entry as `unprobed` and `prune` leaves
  it in place. Investigate the registry directory rather than deleting the
  record by hand (see [`docs/troubleshooting.md`](troubleshooting.md)).
- **Died mid-conversation.** (`kind: "control_unreachable"`, or `"ipc_deadline"` when a bounded window elapsed instead) The registry entry read as live, but the runner
  exited between the liveness check and the reply reaching the client — the
  connect fails, or the connection closes before a complete response. Also a
  bounded `CONTROL` (103) failure, never a wedge: every wait in the control
  plane (connecting, and the request/response exchange) is deadline-bounded.
- **Ambiguous `run_id`.** (`kind: "ambiguous_run_id"`) The registry does not enforce `run_id` uniqueness;
  if more than one **live** entry matches, every by-`run-id` command — the
  read-only `inspect`, `attest`, and `wait` included — fails closed with `CONTROL` (103)
  rather than guessing which entry the scan happened to return first. Keep
  `run_id`s unique among an adapter's own concurrently-live runs (§2) to avoid
  this entirely.
- **An unreadable contract version (`inspect` and `attest`).** (`kind: "incompatible_contract"`) The runner was reached and
  answered, but the reply declared a contract version this client does not read, so
  it was refused instead of being acted on under semantics its sender never
  promised. Both read-only verbs can hit this, each on its own version axis: an
  `inspect` reply carrying a control-plane `snapshot_version` outside the range this
  client reads — newer than the version it implements, or older than the version it
  still decodes — and an `attest` reply carrying an `attestation_version` other than
  the single one this client reads (that axis is read strictly, with no range, since
  a misread membership verdict is a security answer rather than a diagnostic; §4).
  Also a `CONTROL` (103), with a message naming the version that arrived and the
  version — or range — this build reads. Unlike the four above it says nothing about
  the run's liveness: the target is registered, live, reachable, and healthy, and
  the run stays fully controllable, since `cancel`/`kill` acks carry no version and
  `wait`/`list` ask the runner for nothing. Do not treat it as a lost runner or
  retry it; re-run the *same* command with a build that implements the runner's
  version of that contract — for `inspect`, its snapshot version (for a newer
  runner, a build at least as new as the binary that started the run); for `attest`,
  its attestation version. See
  [`docs/control-plane.md`](control-plane.md), "Snapshot version: a newer runner's
  reply is refused, an older one is read" and "Attestation version", and
  [`docs/compatibility.md`](compatibility.md), "Machine-output schemas".
- **A caller the runner cannot name (`attest` only).** (`kind:
  "peer_identity_unsupported"`) The runner was reached and refused to decide
  membership, because its transport could not supply a kernel-authenticated
  identity for the connecting process. Also a `CONTROL` (103), and for the same
  reason as the bullet above: an answer that cannot be trusted is withheld rather
  than guessed — here in the safe direction, since the alternative would be
  reporting an unproven `member`. It says nothing about the run's liveness or
  about membership. Rule it out at preflight by requiring
  `attest:peer-identity` (§1); meeting it at runtime means that check was skipped
  or the runner is a different build.
- **A decided non-membership is *not* in this class.** (`kind: "not_a_member"`,
  exit `NOT_A_MEMBER` 115) When `attest` reports that the caller is not in the
  run's container, nothing failed: the target was resolved, reached, and answered.
  It has a code of its own precisely so an adapter can tell "the runner says no"
  (deny the request) from "no runner said anything" (investigate, or retry). Never
  fold it into the `103` handling above.
- **`CONTROL`-class exit codes are not run outcomes.** A `103` from the
  by-`run_id` form of `inspect`/`cancel`/`kill`/`attest`/`wait` describes a failure
  on the
  *client's* side of the exchange — it could not resolve or reach a single target,
  or (the two read-only cases above) could not read or obtain the answer it asked
  for — and says
  nothing about how the target run itself ended (or is still running). Do not
  conflate it with
  the run-outcome codes in §3's table (`106`–`109`, or the child's own code);
  those come only from the run's own process exit and its `runner_exit`
  event. The same separation applies to `WAIT_TIMEOUT` (112): it is the
  *waiting client* giving up, never the run being stopped (§4).
  `cancel --all` / `kill --all`'s own `103` is the one exception where the
  code *can* coincide with some targets having genuinely been acted on — see
  the `--all` paragraph in §4.
- **A `--detach` exit code is not a run outcome either.** `run --detach`'s `0`
  means "the run started", not "the child succeeded", and its non-zero codes
  mean "the run never started" — carrying the same reserved code the failure
  would have produced in the foreground. An adapter that branches on a detached
  launch's exit code as if it were the child's result will read every
  long-running failure as a success; the child's outcome is in the terminal
  `runner_exit` event (§3), reached after `wait` (§4). See
  [`docs/exit-codes.md`](exit-codes.md#detached-runs-the-code-reports-the-start),
  "Detached runs".
- **`SETUP` (111) vs. `INTERNAL` (104).** (`kind: "setup"` versus `"internal"`; an unreadable *registry* narrows further to `"registry"`) A `run` that could not write its
  `--jsonl`/`--capture-dir`, or open a `--stdin-file`, fails closed with
  `SETUP` (111) — an ordinary, usually-actionable environment problem (bad
  path, permissions), not a runner bug. `INTERNAL` (104) is reserved for a
  genuine invariant violation in the runner's own logic. See "Setup failures
  vs internal faults" in [`docs/exit-codes.md`](exit-codes.md#setup-failures-vs-internal-faults).

## 7. Machine-readable failures: `--error-format json`

Everything in §6 is a real distinction the CLI already makes — but by default an
adapter can only read it as English on stderr, because the exit code is coarse: one
`CONTROL` (103) covers six of those bullets at once — eight `kind` values in all,
since the "died mid-conversation" bullet is two of them (`control_unreachable` and
`ipc_deadline`) and `not_found`, a run id the registry names nowhere, gets no bullet
of its own above. The global, opt-in
`--error-format json` publishes the distinction instead:

```sh
processkit-cli --error-format json inspect --run-id build-42
# stderr, exactly one line:
# {"error_version":1,"code":103,"kind":"stale","operation":"inspect",
#  "run_id":"build-42","retryable":false,"message":"cannot inspect run `build-42`: …"}
```

- **Opt in wherever it is convenient.** The flag is global: it parses before or
  after the subcommand, and every subcommand honors it. Pin it in the preflight
  like any other flag — `--require-surface inspect:--error-format` (§1).
- **Branch on `kind` (and `code`), never on `message`.** `error_version`, `code`,
  `kind`, `operation`, `run_id`, and `retryable` are the contract; `message` is
  free text that may be reworded in any release.
- **`kind` maps onto §6.** `stale`, `unprobed`, `ambiguous_run_id`,
  `control_unreachable`, `ipc_deadline`, `not_found`, and the two refusals
  `incompatible_contract` and `peer_identity_unsupported` — those eight are the
  ones that exist *to split* the single `CONTROL` (103) — plus `not_a_member`
  (the decided verdict, `115`), `host_unqualified` (the other decided verdict,
  `116`, and the one about the host rather than a run — §1),
  `registry`/`setup`/`internal`, `wait_timeout`,
  `events_invalid`, `probe_incompatible`, and — for a failing `run` — the
  terminal `runner_exit` event's own `source` spellings (`spawn_error`,
  `container_error`, `timeout`, `cancelled`, `control_cancel`, `control_kill`,
  `output_overflow`). Unrecognized value? Fall back to `code`; the vocabulary
  grows additively.
- **stdout is untouched.** The envelope is on stderr, so an adapter can leave the
  flag on for every invocation without any risk to the JSON it parses from stdout —
  including for a command that prints a report *and then* fails, such as
  `probe --json` exiting 110 or `inspect --all --json` exiting 103.
- **The default is unchanged.** Without the flag, stderr is byte-for-byte the prose
  every earlier release printed.
- **One documented gap.** clap's *parse-time* usage errors (exit `USAGE`, 100 — an
  unknown flag, a malformed duration, a missing subcommand) stay human-readable in
  v1: they happen before the binary knows what it was asked to do, so there is no
  operation to name. Use the §1 preflight to establish that a flag exists before
  using it. Every **post-parse** failure is covered.

The shape is published like every other machine-readable output in this guide:
`fixtures/schema/cli/error.schema.json` with a golden `error.jsonl` beside it. The
normative field-by-field contract, the full `kind` table, and the `retryable` rule
are in [`docs/exit-codes.md`](exit-codes.md#machine-readable-failures---error-format-json).

## See also

- [`docs/agent-workflows.md`](agent-workflows.md) — a policy and execution
  strategy for automation agents that launch external tools through the runner.
- [`docs/schema.md`](schema.md) — the normative JSONL event schema (every
  field, every event, versioning rules).
- [`fixtures/schema/cli/README.md`](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/fixtures/schema/cli/README.md)
  — the JSON Schema documents and golden fixtures for every machine-readable
  output in this guide (`probe`, `list`, `inspect`, the `cancel`/`kill` acks,
  `prune`, `wait --report-outcome`, `attest`, `doctor`, and the `--error-format json`
  failure
  envelope), and the versioning decision behind them (`probe`, `inspect`, `attest`,
  `doctor`, and the envelope carry their own version field; the other four
  deliberately carry none).
- [`docs/compatibility.md`](compatibility.md) — the compatibility surfaces, the
  pinning procedure, and the upgrade/downgrade checklists.
- [`docs/exit-codes.md`](exit-codes.md) — the normative reserved exit-code
  band and the child-fidelity rule.
- [`docs/control-plane.md`](control-plane.md) — the normative local transport,
  wire protocol, and `inspect`/`cancel`/`kill`/`attest` behavior.
- [`docs/registry.md`](registry.md) — the normative registry location, record
  format, and staleness/reaping rules.
- [`docs/architecture.md`](architecture.md) — the map of this repository's own
  modules, for a contributor rather than a consumer.
- [`docs/troubleshooting.md`](troubleshooting.md) — symptom-to-cause diagnosis
  for an operator, organized by what you observe rather than by call
  sequence.
