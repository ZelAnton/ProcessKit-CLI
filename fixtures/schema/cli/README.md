# CLI machine-output schemas — golden fixtures

This directory publishes, for the CLI's **non-event** machine-readable outputs, the
same three-part contract discipline the JSONL lifecycle-event stream already has
(normative prose + a JSON Schema + a golden fixture):

| Output family | Schema document | Golden fixture | Normative prose |
| --- | --- | --- | --- |
| `probe --json` | `probe.schema.json` | `probe.jsonl` | [`docs/compatibility.md`](../../../docs/compatibility.md), [`docs/integration.md`](../../../docs/integration.md) §1 |
| `list --json` | `list.schema.json` | `list.jsonl` | [`docs/registry.md`](../../../docs/registry.md), "Discovery — `list`" |
| `inspect --json` (single snapshot and the `--all` array) | `inspect.schema.json` | `inspect.jsonl` | [`docs/control-plane.md`](../../../docs/control-plane.md), "`inspect`" |
| `cancel`/`kill` ack (single) and the `--all` report array | `control-ack.schema.json` | `control-ack.jsonl` | [`docs/control-plane.md`](../../../docs/control-plane.md), "The ack", "`cancel --all` / `kill --all`" |
| `prune --json` (tally and `--dry-run`) | `prune.schema.json` | `prune.jsonl` | [`docs/registry.md`](../../../docs/registry.md), "Reaping — `prune`" |
| `wait --report-outcome` (single outcome and the `--all` array) | `wait.schema.json` | `wait.jsonl` | [`docs/registry.md`](../../../docs/registry.md), "Waiting — `wait`" |
| `--error-format json` failure envelope (**stderr**, every subcommand) | `error.schema.json` | `error.jsonl` | [`docs/exit-codes.md`](../../../docs/exit-codes.md), "Machine-readable failures: `--error-format json`"; [`docs/integration.md`](../../../docs/integration.md) §7 |
| `attest --json` containment attestation | `attest.schema.json` | `attest.jsonl` | [`docs/control-plane.md`](../../../docs/control-plane.md), "`attest`" |

**The error envelope is the one family on stderr, and that is the point.** Every
other row above is a *successful* command's stdout. The envelope is what a
**failed** invocation prints, and it goes to stderr precisely so that stdout stays
reserved for success — a command that prints a report and *then* fails
(`probe --json` exiting 110, `inspect --all --json` exiting 103, `attest --json`
exiting 115) leaves its stdout byte-for-byte unchanged and adds the envelope beside
it. It is not another *success* shape; it is the failure-side counterpart to all
seven, which is also why success-output fixtures could never have covered it.

**`attest --json` is the one success-side family whose stdout accompanies a
non-zero exit as a matter of course.** Two of its three verdicts make the invocation
fail, and the attestation is printed for all three: the verdict is the answer the
caller asked for, and the exit code says what to do about it — so both channels
carry the same fact, in the same invocation, without either replacing the other.

**Why `events --json` is not a family here.** It is not a *non-event* output:
`events --json` passes the runner's own JSONL lines through byte for byte, so the
document that describes it is [`fixtures/schema/v1/schema.json`](../v1/schema.json)
itself — the very one this directory exists to complement. Publishing a second
schema for the same bytes would create exactly the parallel contract this
repository avoids. (`events --validate`'s report and the default rendering are
human-readable text, not machine output, and so belong to no family here either.)

Every schema document is JSON Schema **draft 2020-12**, fully self-contained
(internal `$defs` only, no remote `$ref`) — the same convention
[`fixtures/schema/v1/schema.json`](../v1/schema.json) follows, and the reason the
`jsonschema` validator this repository tests with needs no HTTP/file resolver.
Where a family has more than one output form, the document's **root is a `oneOf`**
over named `$defs` (exactly as the event schema's root is a `oneOf` over event
types), so an adapter that knows which invocation it made can point its validator
straight at the specific form instead — for example
`inspect.schema.json#/$defs/snapshot` or `prune.schema.json#/$defs/dryRunReport`.

**The prose documents remain the normative source of truth.** These schemas are a
mechanical mirror of them, kept honest by `tests/machine_output.rs`, which
validates both the golden fixture *and* the real binary's live output against
every document. On a disagreement between a schema and the prose, trust the prose
and treat the schema as needing a fix.

## Versioning: four of these eight are versioned, four deliberately are not

`probe --json` carries `probe_version`, `inspect --json` carries
`snapshot_version`, the failure envelope carries `error_version`, and
`attest --json` carries `attestation_version`, so a bump is
visible in the payload itself. `probe.schema.json`, `error.schema.json` and
`attest.schema.json` pin their value with `const`; `inspect.schema.json`
enumerates a *range* instead — the shape follows the **reader's** tolerance, not a
sibling's precedent, and `inspect` is the one whose client genuinely decodes an
older form (see "The `snapshot_version` range" below). The other four families here
carry no version field, and that was an explicit decision, not an oversight (see
`docs/compatibility.md`, "Machine-output schemas", for the consumer-facing
statement of it).

This project already versions four contracts on four independent axes: the JSONL
`schema_version`, the registry record's `registry_version`, the control-plane
`snapshot_version`, and the probe report's own `probe_version`. What those four
have in common is that **each can be read by a party that did not invoke this
binary**:

- the JSONL stream and the registry record are *durable artifacts* — written now,
  read later, possibly by a different tool or a different version;
- the control-plane snapshot is a *cross-process wire* reply, where the client and
  the runner are two separate processes that can be two separate builds;
- the probe report exists precisely to be read by a consumer that does *not yet
  know* the binary's version — it bootstraps the whole compatibility check.

**`attest --json` meets it on the second of those three counts**, which is why it
arrived versioned: like the `inspect` snapshot, its content is a *cross-process wire
reply* — the verdict originates in the runner, which can be a different build than
the client that prints it. Its pin is `const` rather than a range because the
**reader** is strict: a client refuses any version but its own with `CONTROL` (103)
rather than reading a membership verdict under semantics its sender never promised,
and, unlike `snapshot_version`, this contract has had exactly one version, so
strictness refuses no shape that ever existed. The two version pins that cross a
wire therefore have deliberately different shapes, for a reason that is about their
readers rather than about their senders.

**The failure envelope meets that same test too**, which is why it is versioned
rather than unversioned. It is the only shape here
that a consumer reads *when things went wrong*, and that is exactly when the
invoking context is least likely to be intact: captured stderr sitting in a CI log,
read back hours later by a different tool than the one that ran the binary; a
wrapper script that invoked whatever was on `PATH`; an incident triage holding the
diagnostics but not the invocation. That is a durable artifact read by a party that
did not invoke this binary — the same test the four above pass. `const` rather than
an enumerated range, like `probe_version`: this binary only ever *writes* an
envelope, never reads one, so it has no tolerance window to express.

The remaining outputs published here — `list --json`, `prune --json` (with and
without `--dry-run`), the printed `cancel`/`kill` ack, the `--all` report arrays,
and `wait --report-outcome` — are none of those things. They are **synchronous
stdout renderings consumed by whoever just invoked this exact binary**: the caller
already knows which version produced them, and can pin that version's shape with
the `probe` preflight (`version` plus the `surface` tokens for the exact
subcommand and flags it is about to use). A per-output version field would add a
second, redundant pinning axis without adding information the caller does not
already have, so none was added, and none of these four gained a version field in
the task that published these schemas.

The `cancel`/`kill` ack is the one entry in that list that looks like it belongs
with the versioned contracts instead, since an ack really does travel across a
process boundary. What is **printed** is not that wire message, though. The
client parses the runner's reply into a three-field `ControlAck`, verifies
`accepted`/`action`/`run_id` against the command it sent — failing closed with
`CONTROL` (103) on any mismatch rather than reporting a false success — and
then prints a **fresh serialization of what it
parsed** (`src/control/mod.rs`, `mutate_async`; `inspect` renders its snapshot
the same way). Nothing is passed through byte-for-byte, so a field some newer
runner added is dropped at deserialization and never reaches stdout: do not
expect a mixed deployment's newer runner to surface unknown fields through an
older client, and note that this is precisely why `control-ack.schema.json` can
set `additionalProperties: false` and be *exact* about stdout rather than merely
strict. The printed ack's field set is therefore fixed by the version of the
client — the binary the caller just invoked — which is the same synchronous
rendering the paragraph above describes, needing no version of its own for the
same reason. Should the *wire* ack ever grow a variable shape, that is a
control-plane concern and belongs on the control plane's own existing axis
(`snapshot_version`) rather than on a new one for this printed form.

Two consequences follow, and they are the whole compatibility story for this
directory:

- **A breaking change to any shape here is a major release of the CLI**, exactly
  like a breaking change to a flag (`docs/compatibility.md`, "Compatibility and
  upgrades"). It is announced in `CHANGELOG.md` and the documents in this
  directory are updated **in place** — there is no `vN/` directory here (unlike
  `fixtures/schema/v1/`, whose `v1` *is* the JSONL `schema_version`), because a
  version here, where there is one, lives in the payload rather than in a path.
  For the four unversioned families there is simply no version field for a
  consumer to pin; if a future task ever decides one of them does need its own,
  that is the point at which a versioned directory should appear alongside it.
  For `probe --json`, `inspect --json`, and the failure envelope the pin already
  exists *inside* the document — a breaking change to those shapes bumps
  `probe_version` / `snapshot_version` / `error_version`, and that field, not a
  directory name, is what a consumer checks. For `snapshot_version` the client
  checks it as well, which is the next section.
- **Additive changes stay additive.** A new field on one of these objects, or a
  new value in an open-ended string field, is a minor/patch change; a reader that
  consumes the fields it knows is unaffected. Note that these documents set
  `additionalProperties: false`, which is deliberate for *this repository's own
  drift-detection tests* (an added field must be published here in the same
  commit); an adapter that copies a document into its own pipeline and wants to
  tolerate a future additive field can relax that keyword on its copy.

## The `snapshot_version` range

`inspect.schema.json`'s `snapshot_version` is the one value published in this
directory that the *far side of a wire* supplies: every other field on every other
form — including both other version pins, `probe_version` and `error_version` — is
produced by the binary the caller just invoked, but this number is what the
**runner** declared, echoed unchanged. A run started by an older build therefore
reports that build's number even though the surrounding object is the invoked
binary's own shape (the client re-serializes what it parsed — see the ack discussion
above, which is the same mechanism).

That is why the document enumerates `[1, 2]` rather than pinning `const: 2`, and the
enumeration is exact rather than permissive: the client **acts** on the value before
printing anything. A version newer than it implements is refused with `CONTROL` (103)
instead of being rendered under semantics its sender never promised; a version older
than it still decodes correctly is refused too. What remains — the range between the
floor and the current version, `MIN_READABLE_SNAPSHOT_VERSION..=SNAPSHOT_VERSION` in
`src/control/mod.rs` — is exactly what can reach stdout, so this enumeration moves
whenever either end of that range does, in the same change. The normative statement of
the policy, including why the refusal is one-sided and what moves the floor, is
[`docs/control-plane.md`](../../../docs/control-plane.md), "Snapshot version: a newer
runner's reply is refused, an older one is read".

## The fixtures

Each `*.jsonl` fixture is a **catalog of the forms and variants its schema
document describes** — in the same spirit as
[`fixtures/schema/v1/events.jsonl`](../v1/events.jsonl), which carries one
representative line per event type rather than an exhaustive enumeration of every
value. Where a document has a root `oneOf`, the fixture's lines follow that
order; where a single form has documented variants worth pinning, each gets its
own line:

| Fixture | Lines |
| --- | --- |
| `probe.jsonl` | the healthy `compatible: true` self-report; the fail-closed `compatible: false` report an unmet `--require-*` expectation produces |
| `list.jsonl` | one entry per `health` value — a `live` run publishing every optional field, a confirmed-`stale` leftover publishing none of them, an `unprobed` entry |
| `inspect.jsonl` | the single-run snapshot; the `--all` array with that snapshot inline |
| `control-ack.jsonl` | a `cancel` ack; a `kill` ack; the `cancel --all` report array |
| `prune.jsonl` | the plain tally; the `--dry-run` report with both candidate kinds |
| `wait.jsonl` | a `reported` outcome; an `unknown` one; the `wait --all` report array |
| `error.jsonl` | the variants of the one envelope shape: a named run id versus a `null` one, a retryable verdict versus a final one, four different reserved codes — `not_found`, `stale`, `unprobed`, `probe_incompatible`, `events_invalid`, `not_a_member` |
| `attest.jsonl` | the two verdicts a platform that can name its peers produces: `member` (from a client inside the run) and `not_a_member` (from one outside it) |

`docs/*` and the schema documents remain the complete list — the `already_gone`
and `failed` arms of the `inspect --all` and `cancel`/`kill --all` reports, for
instance, are documented and validated there without a line of their own here.
`attest.jsonl` has one further reason for an unpinned variant: its third verdict,
`peer_identity_unsupported`, can only be produced on a platform whose transport
cannot name a peer, and these lines are generated by the real binary on the platform
running the tests — so pinning it would mean publishing an example no binary here
ever printed. The document describes it and `src/control/mod.rs`'s unit tests drive
it directly.
(The third aggregate array, `wait --all --report-outcome`, has no such extra
arms: its entries are the very `waitOutcome` objects the single-run form prints,
and both of that form's arms already have a line above.) A fixture line is a
representative example an adapter can build and test a reader against.

- **Do not hand-edit these lines.** They are generated from the **real binary's
  output** by `tests/machine_output.rs`, which drives actual runs against an
  isolated scratch registry and captures what the binary actually printed.
  Regenerate after an intentional shape change with:

  ```sh
  UPDATE_MACHINE_SCHEMA_GOLDEN=1 cargo test --test machine_output
  ```

- Two transformations are applied to the captured output before it is written
  here, and to the live output before it is compared against a fixture — both are
  in `tests/machine_output.rs`, both are shape-preserving, and neither ever adds
  or removes a field:
  1. **Canonicalization.** Each line is re-serialized with its object keys sorted,
     so the fixture is stable and diff-friendly and a purely cosmetic field
     reorder in a Rust struct is not a spurious golden failure. JSON object member
     order is not part of any contract here.
  2. **Fixed sample values.** Values that legitimately differ between two runs, or
     between two platforms, are replaced by fixed sample values — timestamps,
     PIDs, absolute paths, the control endpoint, the argv fingerprint, the
     containment mechanism, the member list, the binary's own `version`, and the
     `surface` token list. This is the same convention `events.jsonl` documents
     for its own timestamps, run id, and PIDs; a real invocation carries the
     actual values. The **shapes** (which fields exist, their types, whether they
     are null) are never normalized — those are exactly what the fixture pins.

- `error.jsonl`'s `message` is replaced by a fixed sample for a different reason
  than everything in (2) above: it is not *volatile*, it is **not part of the
  contract**. `error.schema.json` says so, and the fixture is where that decision
  is enforced rather than merely stated — pinning the prose would turn every
  reworded diagnostic into a golden conflict while guarding nothing a consumer is
  allowed to depend on. Everything a consumer *is* allowed to depend on
  (`error_version`, `code`, `kind`, `operation`, `run_id`, `retryable`) is pinned
  exactly, and the real message is still schema-validated on the live output like
  every other field.

- `probe.jsonl`'s `surface` is truncated to a fixed two-token sample on purpose.
  The full token list is already pinned exhaustively — by the `fixtures/cli-help/`
  golden snapshots (`docs/compatibility.md`, "Compatibility and upgrades") and by
  `probe`'s own unit test — so repeating all of it in a second golden would make
  every new flag churn this fixture without guarding anything new. The live
  report's *real*, complete `surface` is still validated against
  `probe.schema.json` on every test run.

- Closed vocabularies are enumerated in these documents where they are small and
  stable (`health`, `mechanism`, the `status` sets, the ack `action`, the
  envelope's `kind` and `operation`). The
  terminal-event `source` vocabulary that `wait --report-outcome` echoes is
  deliberately **not** duplicated here: it grows additively, and
  [`fixtures/schema/v1/schema.json`](../v1/schema.json)'s `runnerExit.source` is
  its single source of truth.

- **`error.schema.json`'s `kind` enum is the one place that rule is bent, on
  purpose and under a test.** A failing `run` reports a `kind` spelled exactly like
  the `runnerExit.source` value for the same ending (`spawn_error`,
  `container_error`, `timeout`, `cancelled`, `control_cancel`, `control_kill`,
  `output_overflow`, `setup`, `internal`), because the alternative — inventing a
  second set of names for endings that already have published ones — is precisely
  the parallel contract this directory exists to avoid. The reuse is
  one-directional: `runnerExit.source` remains the source of truth for those
  spellings, the envelope mirrors it, and `src/error_envelope.rs`'s
  `the_run_family_kinds_are_spelled_exactly_as_the_event_streams_source_values`
  reads both documents off disk and fails if they ever drift apart. The rest of the
  `kind` vocabulary (the CONTROL/SETUP refinements, `wait_timeout`,
  `events_invalid`, `probe_incompatible`, `usage`, `unknown`) belongs to the
  envelope alone and appears nowhere else — with one deliberate exception in the
  other direction: `not_a_member` and `peer_identity_unsupported` are spelled exactly
  like two of `attest.schema.json`'s own `verdict` values, because they name the same
  two facts. The envelope reports the *consequence* of a verdict the attestation
  itself already stated, so a second spelling for it would be the same parallel
  contract in miniature. The third verdict, `member`, has no envelope counterpart at
  all: it is a success, and successes have no failure to describe.

## What the tests guarantee

`tests/machine_output.rs` asserts, per output family:

1. the committed fixture parses, and every line validates against the family's
   schema document;
2. the **real binary's** output for that family — captured from an actual
   invocation, un-normalized — validates against the same document; and
3. the normalized form of that real output matches the committed fixture
   byte-for-byte.

(1) keeps the published example honest, (2) is the drift guard proper (the
documents set `additionalProperties: false` and list every field in `required`, so
a field added to or removed from a Rust struct fails the test until the schema is
updated with it), and (3) keeps the fixture representative of what the binary
actually prints today.

For the `error` family, "the real binary's output" is read from **stderr** rather
than stdout, and the same test additionally asserts that stdout carries no envelope
at all — the invariant that lets an adapter turn `--error-format json` on
permanently. Its behavioral half (the flag's position independence, the unchanged
default prose, and the kinds no fixture line pins) lives in
`tests/error_envelope.rs`, and its vocabulary is held against `src/error_envelope.rs`
by that module's own unit tests: one asserts the schema's `kind` enum lists exactly
the kinds the build can emit, in the same order, so a kind can never be added in
code without being published here.
