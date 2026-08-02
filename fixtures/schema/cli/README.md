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
| `wait --report-outcome` | `wait.schema.json` | `wait.jsonl` | [`docs/registry.md`](../../../docs/registry.md), "Waiting — `wait`" |

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

## Versioning: these outputs are deliberately unversioned

This was an explicit decision, not an oversight (see `docs/compatibility.md`,
"Machine-output schemas", for the consumer-facing statement of it).

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

The remaining outputs published here — `list --json`, `prune --json` (with and
without `--dry-run`), the printed `cancel`/`kill` ack, the `--all` report arrays,
and `wait --report-outcome` — are none of those things. They are **synchronous
stdout renderings consumed by whoever just invoked this exact binary**: the caller
already knows which version produced them, and can pin that version's shape with
the `probe` preflight (`version` plus the `surface` tokens for the exact
subcommand and flags it is about to use). A per-output version field would add a
second, redundant pinning axis without adding information the caller does not
already have, so none was added, and none of these outputs gained a version field
in the task that published these schemas.

Two consequences follow, and they are the whole compatibility story for this
directory:

- **A breaking change to any shape here is a major release of the CLI**, exactly
  like a breaking change to a flag (`docs/compatibility.md`, "Compatibility and
  upgrades"). It is announced in `CHANGELOG.md` and the documents in this
  directory are updated **in place** — there is no `vN/` directory here (unlike
  `fixtures/schema/v1/`, whose `v1` *is* the JSONL `schema_version`), because
  there is no version field for a consumer to pin. If a future task ever decides
  one of these outputs does need its own version field, that is the point at which
  a versioned directory should appear alongside it.
- **Additive changes stay additive.** A new field on one of these objects, or a
  new value in an open-ended string field, is a minor/patch change; a reader that
  consumes the fields it knows is unaffected. Note that these documents set
  `additionalProperties: false`, which is deliberate for *this repository's own
  drift-detection tests* (an added field must be published here in the same
  commit); an adapter that copies a document into its own pipeline and wants to
  tolerate a future additive field can relax that keyword on its copy.

The one shape here that *does* cross a process boundary is the `cancel`/`kill`
**ack**: it is the runner's reply as well as the client's stdout. It stays
unversioned too, deliberately — it is a three-field, self-identifying reply
(`accepted`/`action`/`run_id`) that the client verifies field by field before
printing, failing closed with `CONTROL` (103) on any mismatch rather than
reporting a false success. If it ever grows a variable shape, it belongs on the
control plane's existing `snapshot_version` axis rather than on a new one.

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
| `wait.jsonl` | a `reported` outcome; an `unknown` one |

`docs/*` and the schema documents remain the complete list — the `already_gone`
and `failed` arms of the two aggregate reports, for instance, are documented and
validated there without a line of their own here. A fixture line is a
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

- `probe.jsonl`'s `surface` is truncated to a fixed two-token sample on purpose.
  The full token list is already pinned exhaustively — by the `fixtures/cli-help/`
  golden snapshots (`docs/compatibility.md`, "Compatibility and upgrades") and by
  `probe`'s own unit test — so repeating all of it in a second golden would make
  every new flag churn this fixture without guarding anything new. The live
  report's *real*, complete `surface` is still validated against
  `probe.schema.json` on every test run.

- Closed vocabularies are enumerated in these documents where they are small and
  stable (`health`, `mechanism`, the `status` sets, the ack `action`). The
  terminal-event `source` vocabulary that `wait --report-outcome` echoes is
  deliberately **not** duplicated here: it grows additively, and
  [`fixtures/schema/v1/schema.json`](../v1/schema.json)'s `runnerExit.source` is
  its single source of truth.

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
