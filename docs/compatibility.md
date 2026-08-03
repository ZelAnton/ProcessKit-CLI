# Compatibility and upgrades

ProcessKit CLI has three public compatibility surfaces:

1. command names and flags;
2. the reserved runner exit-code band;
3. JSONL `schema_version`.

The human CLI surface is guarded by through-binary golden snapshots for the root
help and every public subcommand in `fixtures/cli-help/`. An intentional flag,
value-name, default, or help change must be regenerated with
`UPDATE_CLI_HELP_GOLDEN=1 cargo test --test cli_help` and the fixture diff reviewed.
The test normalizes only the Windows `.exe` suffix and line endings; all contract
text and ordering remain exact.

Breaking any of them requires a major release. An adapter should verify the
exact pieces it uses before launching a payload rather than discovering an
incompatible binary after work has started.

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

## Surface tokens

A surface token is either a subcommand or `subcommand:--long-flag`:

```text
run
run:--jsonl
run:--idle-timeout
inspect
inspect:--json
cancel:--all
```

Require only the features the adapter will actually use. This permits additive
CLI releases while preventing an invocation from reaching a binary that lacks
a needed flag.

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

1. **New event types.** Route by the `event` discriminator and ignore a type you
   do not know, rather than failing on it or assuming the stream is corrupt.
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
4. **New values in an open-ended descriptive string field** — a new `cancelled`
   `source`, a new `runner_exit` `source`, a new `hint` label. Treat an unrecognized
   value as "some other trigger" and keep routing by event type.
5. **Unknown fields anywhere in the envelope or event body.**

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
| `wait --report-outcome` | `fixtures/schema/cli/wait.schema.json` | `wait.jsonl` |

`events --json` is deliberately absent from that table: it passes the runner's own
JSONL lines through byte for byte, so the document that describes it is the event
schema above (`fixtures/schema/v1/schema.json`), not a second one of its own. That
is also what `events --validate` checks a stream against.

`tests/machine_output.rs` validates the real binary's output for each of these
against its document on every test run, so an accidental shape change fails CI
instead of reaching an adapter. A document whose family has more than one output
form has a root `oneOf` over named `$defs`, so a consumer can validate against the
exact form it invoked — for example `inspect.schema.json#/$defs/snapshot`.

**Two rows of that table carry a version field; the other four deliberately do
not.** `probe --json` carries `probe_version` and `inspect --json` carries
`snapshot_version` — the same field the runner puts on the control-plane wire.
`probe.schema.json` pins its value with `const`; `inspect.schema.json` admits the
range of snapshot versions this build renders (see the `snapshot_version` bullet
below), because that field reports the *runner's* contract, not the invoked
binary's. Either way a bump is visible in the payload itself. **Pin those two on
their own version field**, not on the CLI version alone: ignoring a
`snapshot_version` bump across an upgrade is exactly the class of mistake this
section exists to prevent. Both
are versioned for the reason the project's other two versioned contracts (the
durable JSONL stream's `schema_version` and the registry record's
`registry_version`) are: each can be read by a party that did not invoke the
binary — the snapshot crosses a process boundary to a runner that may be a
different build, and the probe report's whole job is to be read *before* the
binary's version is known.

**The remaining four — `list --json`, the `cancel`/`kill` ack and `--all` report,
`prune --json`, and `wait --report-outcome` — carry no version field,
deliberately.** Each is a synchronous stdout rendering read by the caller that
just invoked this exact binary. That includes the printed ack, whose content does
arrive over the wire but is re-serialized by the client from the three fields it
parsed and verified, so its field set is the client's own (see
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
- **`probe --json` and `inspect --json` additionally bump their own field.** A
  breaking change to either shape bumps `probe_version` / `snapshot_version`
  respectively, and that field is what a consumer should check. For the snapshot,
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

## See also

- [Integration guide](integration.md) — complete adapter lifecycle.
- [Exit-code contract](exit-codes.md).
- [JSONL event schema](schema.md).
- [Platform support](platform-support.md).
