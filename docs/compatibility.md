# Compatibility and upgrades

ProcessKit CLI has three public compatibility surfaces:

1. command names and flags;
2. the reserved runner exit-code band;
3. JSONL `schema_version`.

**Three, and only three — a versioned payload is not a fourth surface.** Several
machine-readable outputs carry a version field of their own: `probe --json`'s
`probe_version`, `inspect`'s `snapshot_version`, the `--error-format json`
failure envelope's `error_version`, and `attest --json`'s `attestation_version`.
Each of those *rides on* the list above — on the
command and flag that produce it (surface 1), and, for the failure envelope and the
attestation, on the
reserved code it reports (surface 2) — and pins its own shape inside the payload
rather than adding a further thing to pin before launching. (Count them separately:
"three compatibility surfaces" and "how many version fields this project publishes"
are different questions with different answers.) The registry record's
`registry_version` is not on the list either, for a different reason: the per-user
registry is a private contract between this binary and itself, not something a caller
reads off an invocation (see [`docs/registry.md`](registry.md)). Which outputs carry a
version field, and why the rest deliberately do not, is "Machine-output schemas"
below.

The human CLI surface is guarded by through-binary golden snapshots for the root
help and every public subcommand in `fixtures/cli-help/`. An intentional flag,
value-name, default, or help change must be regenerated with
`UPDATE_CLI_HELP_GOLDEN=1 cargo test --test cli_help` and the fixture diff reviewed.
The test normalizes only the Windows `.exe` suffix and line endings; all contract
text and ordering remain exact.

Breaking any of those three surfaces requires a major release. An adapter should
verify the exact pieces it uses before launching a payload rather than discovering
an incompatible binary after work has started.

## Fail-closed preflight

```sh
processkit-cli probe --json \
  --require-schema-version 1 \
  --require-exit-code-band 100-119 \
  --require-surface run \
  --require-surface run:--jsonl \
  --require-surface run:--capture-dir \
  --require-surface inspect:--json \
  --require-surface cancel
```

`probe` is side-effect-free. It launches no child, creates no container, and
does not touch the registry. A missing requirement exits
`PROBE_INCOMPATIBLE` (`110`) and reports all mismatches.

That answers a question about the **binary**. The matching question about the
**host** — can this machine actually create the registry, contain a process, and
round-trip the control plane? — is what `doctor` answers, by doing all three in a
bounded scratch run and reporting what it observed; an unmet host requirement exits
`HOST_UNQUALIFIED` (`116`). It is a setup-time check rather than a per-launch one,
because unlike `probe` it has real (self-cleaning) side effects. See
[`docs/integration.md`](integration.md) §1 and
[`docs/troubleshooting.md`](troubleshooting.md), "Qualifying a host: `doctor`".

## Surface tokens

A surface token takes one of three forms — a subcommand, `subcommand:--long-flag`,
or `subcommand:capability`:

```text
run
run:--jsonl
run:--idle-timeout
inspect
inspect:--json
cancel:--all
attest:peer-identity
run:resource-summary
```

The first two forms name a **spelling** the parser accepts, and are derived from the
live clap definition so they cannot drift from the real one. The third names a
**capability** instead — "can this binary *do* the thing", which no parser can
answer — and is told apart by carrying **no `--`**. The missing `--` is what keeps the
two categories from being read as one
another, so an adapter that validates this array with a pattern of its own must
admit the `--`-less form; the published grammar is the `surface` pattern in
`fixtures/schema/cli/probe.schema.json`.

There are two capability tokens, and they differ in *why* they might be absent:

- **`attest:peer-identity` — a platform capability.** Published exactly where this
  build can obtain a kernel-authenticated identity for a control-plane client, which is
  what makes `attest` able to return a membership verdict at all (see
  [`docs/control-plane.md`](control-plane.md) and
  [`docs/integration.md`](integration.md), §1). A given target may lack it.
- **`run:resource-summary` — a build capability.** Published by every build whose `run`
  emits the terminal `resource_summary` event (see
  [`docs/schema.md`](schema.md#resource_summary)). No platform removes it; only an
  older binary lacks it. That is exactly why it needs a token: an event's presence is
  otherwise undiscoverable until a run has already finished without it, which is after
  the work the number was wanted for.

A consumer requires either with the same `--require-surface` and does not need to know
which kind it is holding.

A capability token's **presence is a guarantee**, and it is worth being precise about
*what* is guaranteed in each case. `attest:peer-identity` guarantees this target names
the peer, so a negative membership answer from it is a real verdict rather than a
missing capability in disguise. `run:resource-summary` guarantees the event will be in
the stream — **not** that any particular measurement in it is populated: which axes
carry numbers is a property of the containment mechanism (`run_started.mechanism`) and,
for memory and CPU on Linux cgroup v2, of how the run ended. The normative matrix is in
[`docs/resource-limits.md`](resource-limits.md#what-the-tree-consumed). A single token
could not honestly carry five per-platform facts, and splitting it into five would
publish as capabilities what are really documented properties of the mechanism — which
is also why no token could have promised numbers a *read point* decides.

A capability's **absence withholds its guarantee rather than predicting failure** —
`attest` on a build without the token still answers from whatever the kernel actually
provides, and fails closed with `peer_identity_unsupported` when that is nothing.
Requiring it therefore turns "this platform cannot prove membership" into an ordinary
`PROBE_INCOMPATIBLE` (110) at preflight, instead of a refusal in the middle of a job.

Require only the features the adapter will actually use. This permits additive
CLI releases while preventing an invocation from reaching a binary that lacks
a needed flag. That is why the preflight example above pins no capability token: it
shows an adapter that uses `run`, `inspect`, and `cancel` and nothing else. An
adapter that will gate work on containment membership adds
`--require-surface attest:peer-identity` to its own invocation — an adapter that will
not must leave it out, since requiring an unused capability would fail preflight on a
platform whose missing capability could not have affected it.

## Schema pinning

Every event carries `schema_version`. An adapter should reject an unknown value
before interpreting event-specific fields. The current schema is documented in
[JSONL event schema](schema.md) and available from the installed binary:

```sh
processkit-cli probe --json --print-schema > schema.json
```

`--print-schema` is mutually exclusive with `--require-*`: printing the
document and checking compatibility are separate operations, so neither can be
silently skipped.

### What a reader must tolerate within one version

Within one schema version, a reader must tolerate every one of the following.
Removing a field, renaming it, or changing the meaning or type of an existing one
is what requires a new schema version — nothing below does.

1. **New event types — including ones that appear in *every* run.** Route by the
   `event` discriminator and ignore a type you
   do not know, rather than failing on it or assuming the stream is corrupt. Note the
   scope of this obligation carefully: a new type need not be gated behind a flag.
   `resource_summary` (see [`docs/schema.md`](schema.md#resource_summary)) is the worked
   example — it is emitted by every run that spawned a child, so a default `run`'s
   stream is one line longer than an earlier v1 build's. Nothing existing changed, which
   is what makes it additive; a reader that pinned the exact *set* of event types a run
   emits, rather than routing by tag, is the only kind that notices.
2. **More occurrences of an event type you already know**, including a type that
   previously occurred at most once. Within a version you may **not** assume any
   event type is unique, or that its position in the stream is fixed relative to
   other types beyond what the ordering contract states. `members_snapshot` is the
   worked example: it appeared exactly once per run until `run --snapshot-interval`
   made a run emit it on a cadence.
3. **New fields on an event you already parse — including always-present ones,
   not only optional ones.** "Additive" here means "no existing field changes",
   not "the new field may be absent": `members_snapshot` gained an always-present
   `reason` and an always-present `read_error`, `timeout` gained an always-present
   `reason`, and `cleanup_started`/`cleanup_finished` gained always-present
   `read_error` flags, all within version 1. A reader must therefore consume the
   fields it uses rather than pin an event's exact field set — validating with
   `additionalProperties: false` against a copy of a published document will fail
   on the next additive release (see "Machine-output schemas" below).
   The new `limit_evidence` event is likewise additive: readers should route by
   `event`, ignore the new type when they do not use resource-limit attribution,
   and keep `schema_version` pinned at `1`.

   A requested cap that fails during `ProcessGroup` creation remains the
   pre-spawn `limit_hit` path. Since no group exists on that path, it has no
   `limit_evidence` event; readers must not synthesize an `unknown` evidence
   record for it.

   The `resource_summary` event carries the same obligation in a stronger form,
   because *every* one of its measurements is nullable: a `null` means "this
   mechanism, at this read point, does not account for it" and must not be read as, or
   replaced by, `0`. Check its `read_error` flag before drawing any conclusion from a
   `null` — an all-`null` summary is a correct reading on a mechanism with no whole-tree
   accounting, and equally on a flagless Linux cgroup v2 run whose child exited on its
   own; only that flag distinguishes either from a read that failed. The per-axis
   platform matrix is in
   [`docs/resource-limits.md`](resource-limits.md#what-the-tree-consumed) — read it
   before relying on an axis, because two of them are governed by the read point and not
   by the platform alone — and its two IO counters are explicitly **not** comparable
   across platforms.
4. **New values in an open-ended descriptive string field** — a new `cancelled`
   `source`, a new `runner_exit` `source`, a new `hint` label. Treat an unrecognized
   value as "some other trigger" and keep routing by event type.
5. **New values in the growing `mechanism` vocabulary.** `process_reaper` is an
   additive value within schema v1: no existing mechanism changed meaning, and
   readers must treat an unfamiliar mechanism as an unsupported containment choice
   rather than reject the complete event or machine-output record. `unknown` is an
   intentional contract value emitted by the CLI's conservative fallback for a
   future `processkit::Mechanism` variant, not a projection error. The published
   schemas enumerate the current vocabulary, so an older frozen schema may reject a
   newer value; strict validation must use the producer-matching schema, and an
   adapter's runtime parser must not treat the old enum as exhaustive.
6. **Unknown fields anywhere in the envelope or event body.**

**The normative source is [`docs/schema.md`](schema.md)**, not this page: its
"Ordering" section (and specifically "Multiplicity of `members_snapshot`") states
how many of each event a stream may carry and where, and its "Versioning" section
defines exactly which changes are additive and which are breaking. This section
restates those obligations for an adapter author; where the two ever disagree,
`docs/schema.md` wins and the disagreement is a bug in this page.

## Machine-output schemas

The JSONL stream is not the only machine-readable output. The discovery and
control commands print machine-readable JSON to stdout too, and each of those
shapes has a published JSON Schema (draft 2020-12, self-contained) plus a golden
fixture under
[`fixtures/schema/cli/`](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/fixtures/schema/cli/README.md):

| Output | Schema document | Golden fixture |
| --- | --- | --- |
| `probe --json` | `fixtures/schema/cli/probe.schema.json` | `probe.jsonl` |
| `list --json` | `fixtures/schema/cli/list.schema.json` | `list.jsonl` |
| `inspect --json`, `inspect --all --json` | `fixtures/schema/cli/inspect.schema.json` | `inspect.jsonl` |
| `cancel`/`kill` ack, `cancel --all`/`kill --all` report | `fixtures/schema/cli/control-ack.schema.json` | `control-ack.jsonl` |
| `prune --json`, `prune --dry-run --json` | `fixtures/schema/cli/prune.schema.json` | `prune.jsonl` |
| `wait --report-outcome`, `wait --all --report-outcome` | `fixtures/schema/cli/wait.schema.json` | `wait.jsonl` |
| the `--error-format json` failure envelope (stderr, any subcommand) | `fixtures/schema/cli/error.schema.json` | `error.jsonl` |
| `attest --json` | `fixtures/schema/cli/attest.schema.json` | `attest.jsonl` |
| `doctor --json` | `fixtures/schema/cli/doctor.schema.json` | `doctor.jsonl` |

The failure-envelope row is the odd one out in two ways, both deliberate. It is
printed on
**stderr**, not stdout — that is what keeps stdout reserved for successful output,
so a command that prints a report and then fails leaves its stdout untouched — and
it describes a *failure* rather than a success, which is exactly why the eight
success shapes beside it could never have covered it. It is opt-in: without
`--error-format json`, a failure prints the same free-text prose it always did.

Two of the table's rows are *verdict* shapes, and both are routinely printed
**alongside** a non-zero exit, with no flag involved in either. Two of
`attest --json`'s three
verdicts make the command fail, and the attestation is printed for all three (see
[`docs/control-plane.md`](control-plane.md), "`attest`"); `doctor --json` exits
`HOST_UNQUALIFIED` (116) whenever a phase failed or a `--require-*` expectation about
the host went unmet, and prints the same report either way (see
[`docs/integration.md`](integration.md) §1 and
[`docs/troubleshooting.md`](troubleshooting.md), "Qualifying a host: `doctor`"). In
both cases the verdict is the answer and the exit code only says what to do about it,
so neither channel replaces the other: **do not treat a non-zero exit as "there is no
stdout to parse"**. (`probe --json` behaves the same way but only when the caller
asked for a `--require-*` expectation the binary did not meet, and `inspect --all`'s
and `cancel`/`kill --all`'s report arrays precede a `CONTROL` (103) that reports what
could not be *done* rather than what was *decided* — a different fact, the same
reading discipline. `wait --report-outcome` is neither: it prints only when the wait
succeeded.)

`events --json` is deliberately absent from that table: it passes the runner's own
JSONL lines through byte for byte, so the document that describes it is the event
schema above (`fixtures/schema/v1/schema.json`), not a second one of its own. That
is also what `events --validate` checks a stream against.

`tests/machine_output.rs` validates the real binary's output for each of these
against its document on every test run, so an accidental shape change fails CI
instead of reaching an adapter. A document whose family has more than one output
form has a root `oneOf` over named `$defs`, so a consumer can validate against the
exact form it invoked — for example `inspect.schema.json#/$defs/snapshot`.

**Five rows of that table carry a version field; the other four deliberately do
not.** `probe --json` carries `probe_version`, `inspect --json` carries
`snapshot_version` — the same field the runner puts on the control-plane wire —
the failure envelope carries `error_version`, `attest --json` carries
`attestation_version`, and `doctor --json` carries `doctor_version`.
`probe.schema.json`, `error.schema.json`, `attest.schema.json`, and
`doctor.schema.json` pin their value
with `const`;
`inspect.schema.json` admits the
range of snapshot versions this build renders (see the `snapshot_version` bullet
below), because that field reports the *runner's* contract, not the invoked
binary's. The shape follows the **reader's** tolerance rather than a sibling's
precedent: `attest`'s value also comes off the wire, and is pinned anyway, because
its client refuses any version but its own outright — a membership verdict read
under unpromised semantics is worse than no verdict. Either way a bump is visible in
the payload itself. **Pin those five on
their own version field**, not on the CLI version alone: ignoring a
`snapshot_version` bump across an upgrade is exactly the class of mistake this
section exists to prevent. All five
are versioned for the reason the project's other two versioned contracts (the
durable JSONL stream's `schema_version` and the registry record's
`registry_version`) are: each can be read by a party that did not invoke the
binary — the snapshot and the attestation cross a process boundary to a runner that
may be a different build, the probe report's whole job is to be read *before* the
binary's version is known, a failure envelope is routinely read out of its
invoking context altogether (captured stderr in a CI log, read back later by a
different tool than the one that ran the binary), and a `doctor` report is meant to
be *kept*: a failed qualification writes it into the diagnostics directory it leaves
behind, precisely so it can be read later, elsewhere, by whoever is debugging the
host rather than by whoever ran it.

**The remaining four — `list --json`, the `cancel`/`kill` ack and `--all` report,
`prune --json`, and `wait --report-outcome` in both its forms — carry no version
field, deliberately.** Each is a synchronous stdout rendering read by the caller
that just invoked this exact binary. That includes the printed ack, whose content
does arrive over the wire but is re-serialized by the client from the three
fields it parsed and verified, so its field set is the client's own (see
[`fixtures/schema/cli/README.md`](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/fixtures/schema/cli/README.md),
"Versioning"). Such a caller already knows the version and can pin the shape
through the `probe` preflight above — the reported `version`, plus one
`--require-surface` token per subcommand and flag it will actually use. A
per-output version field would be a second, redundant pinning axis, so none was
added.

Consequently:

- **The unversioned four ride on the *first* compatibility surface** (command
  names and flags), not on a version integer of their own. A breaking change to
  any of them — removing a field, renaming it, changing its type or the meaning
  of a value — is a **major** release, announced in the changelog.
- **`probe --json`, `inspect --json`, the failure envelope, `attest --json`, and
  `doctor --json` additionally bump their own field.** A
  breaking change to either of the first two shapes bumps `probe_version` /
  `snapshot_version` respectively, and that field is what a consumer should check.
  The envelope's `error_version` works the same way: removing or re-typing one of
  its stable fields, or changing what an existing `kind` means, bumps it, while a
  new field or a new `kind` value is additive and does not (see
  [`docs/exit-codes.md`](exit-codes.md#machine-readable-failures---error-format-json),
  "Machine-readable failures", and the "Stability" section there). For the snapshot,
  this binary's own `inspect` client checks it too, and does so **asymmetrically**:
  a runner answering with a `snapshot_version` *newer* than this build implements
  is refused with `CONTROL` (103) rather than rendered under semantics its sender
  never promised, while an older one is still read for as long as this build
  genuinely decodes it (today: version 1, the version every release so far writes).
  See [`docs/control-plane.md`](control-plane.md), "Snapshot version: a newer
  runner's reply is refused, an older one is read", for the rule, the floor, and
  what moves it. Two consequences for an upgrade: a bump is a hard boundary for
  *older* clients in a mixed deployment, not merely a signal to read; and the
  `snapshot_version` on stdout is the runner's number, so `inspect.schema.json`
  admits the range this build renders instead of pinning a single value.
  `attest --json`'s `attestation_version` is checked too and is the strict case of
  the same rule — any version but this build's own is refused, because a membership
  verdict must never be read under semantics its sender did not promise, and because
  that contract has had only one version so far, so strictness refuses nothing that
  ever existed. Whether a wire-supplied version pin is a range or a single value
  follows what its **reader** actually decodes, never what a sibling document does.
- **Every document under `fixtures/schema/cli/` is updated in place.** There is no
  `vN/` directory there (unlike `fixtures/schema/v1/`, whose `v1` *is* the JSONL
  `schema_version`), because a version here, where there is one, lives in the
  payload rather than in a path. If a future release gives one of the unversioned
  four a version field of its own, that is the point to revisit the layout.
- **Additive changes remain minor/patch**, exactly as they are for the JSONL
  stream, and a reader that consumes the fields it knows is unaffected.

The published documents set `additionalProperties: false` so that this repository's
own tests fail when a field is added without publishing it. An adapter that copies a
document into its own pipeline and wants to tolerate a future additive field should
relax that keyword on its copy rather than pin a field set the project treats as
additive.

## Exit-code compatibility

Normal foreground runs forward the child code. Runner-owned outcomes occupy
`100`–`119`. Because a child may itself return a number inside that band, the
terminal `runner_exit.child_code` / runner reason remains the authoritative
machine distinction.

Verify the whole reserved band, not only the individual codes your current
version knows. This protects space for additive runner outcomes without making
an old adapter classify them as child exits.

Detached launch is the documented exception: launcher `0` means the run
started, and the child's eventual code is in terminal JSONL.

## Upgrade procedure

1. Download and verify the new archive without replacing the active binary.
2. Run the new binary by absolute path with the adapter's full `probe`
   requirements.
3. Compare `probe --print-schema` with the adapter's pinned schema version.
4. Exercise one harmless run through the same I/O, capture, and control-plane
   flags used in production.
5. Confirm `run_started.mechanism` in the actual deployment environment.
6. Atomically replace the installed binary.
7. Keep the previous binary until terminal events from the smoke run have been
   consumed successfully.

## Reading release notes

The [project changelog](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/CHANGELOG.md)
uses Keep a Changelog categories. Look for:

- `Added`: optional flags/events/features an adapter may adopt;
- `Changed`: behavior that remains inside the current compatibility contract;
- `Fixed`: corrections that may alter previously buggy observations;
- `Removed`: major-version migration work.

The manifest version, `v<version>` tag, crates.io artifact, and GitHub Release
are produced by one release workflow and should identify the same release.

## Rolling upgrades with live runs

The control plane lives inside each running process. Replacing the executable
on disk does not upgrade a runner already in memory. A new client must therefore
remain compatible with the live run's registry/control/schema contract until
that run finishes.

For a conservative rolling upgrade:

1. stop launching new work through the old binary;
2. let or cancel old live runs;
3. wait for the registry to become empty;
4. preview and prune stale entries;
5. replace the binary and launch new work.

Do not use recorded PIDs to bridge versions.

## Downgrades

Run the target older binary's `probe` by absolute path before replacing the
newer one. An older binary may lack an additive flag even when the JSONL schema
version is unchanged. Surface-token verification catches that case.

Preserve event files across a downgrade; they are durable observations from the
binary that produced them and must be decoded according to their own
`schema_version`, not the currently installed executable.

The rolling-upgrade and downgrade procedures above are not only described here.
A scheduled, non-gating workflow (`.github/workflows/interop.yml`) downloads the
latest published release and runs both directions of them against the current
build every week — new clients over an old runner and the reverse, an abandoned
record reaped from either side, `probe` pinning both ways, and each binary's
JSONL stream read under the other's schema. See `CONTRIBUTING.md`,
"Cross-version interop".

## Adapter acceptance checklist

- exact supported `schema_version`;
- exact reserved exit-code band;
- every command and long flag the adapter invokes;
- required containment `mechanism`;
- required `abrupt_cleanup` strength;
- I/O mode/capture assumptions;
- resource-limit applicability in the real environment.

The last four are properties of the *deployment*, not of the binary, so `probe`
cannot answer them: confirm them on each host with `doctor`
(`--require-mechanism`, `--require-abrupt-cleanup`, `--check-resource-controller
--require-resource-controller`), which reports what the machine actually did rather
than what its platform can do in principle.

## See also

- [Integration guide](integration.md) — complete adapter lifecycle.
- [Exit-code contract](exit-codes.md).
- [JSONL event schema](schema.md).
- [Platform support](platform-support.md).
