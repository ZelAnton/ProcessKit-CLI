# JSONL schema v1 — golden fixtures

This directory holds two companion artifacts for schema version 1 of
processkit-cli's JSONL lifecycle-event contract: the golden sample stream
(`events.jsonl`) and its machine-readable JSON Schema (`schema.json`).

`events.jsonl` is the **golden sample stream** for schema version 1 of
processkit-cli's JSONL lifecycle-event contract: at least one representative
instance of every event type, serialized exactly as the runner writes each line to
a `--jsonl` file. A few event types appear more than once to pin their documented
variants — `run_started` redacted vs `--argv-raw`, and the `cancelled` event's
`ctrl_c` vs `control_cancel` sources — and the control-plane endings (`control_cancel`
/ `killed` / `control_kill`) are appended after the base catalog so an additive
extension never rewrites an existing shipped line. Adapters (for example the
processkit-py CLI) that pin `schema_version` can use it as the reference material to
build and test their readers against.

The normative field-by-field description lives in
[`docs/schema.md`](../../../docs/schema.md). This fixture is the machine-readable
companion: the golden test in `src/events.rs`
(`events::tests::golden_stream_matches_the_fixture`) renders the same sample
events and asserts they match this file byte-for-byte, so any accidental change to
an event's wire shape fails the build.

- **Do not hand-edit** these lines. They are generated. Regenerate after an
  intentional schema change (an additive one, or a version-bumped breaking one) with:

  ```sh
  UPDATE_SCHEMA_GOLDEN=1 cargo test --bin processkit-cli \
      events::tests::golden_stream_matches_the_fixture
  ```

- The timestamps, run id, and PIDs here are fixed sample values chosen for a
  stable fixture; a real stream carries the actual run's values.
- A breaking change to any shape below is a **major** bump of `schema_version`
  (see `docs/schema.md`, "Versioning"), which lands under a new `fixtures/schema/vN/`.

`schema.json` is a JSON Schema (draft 2020-12) mechanically transcribed from
`docs/schema.md`, published for adapters that would rather validate against a
schema than reimplement the shapes by hand. **`docs/schema.md` remains the
normative source of truth**; `schema.json` is kept honest against it (and
against `events.jsonl`, and several live streams emitted by the through-the-
binary tests) by `tests::golden_fixture_validates_against_the_schema` and its
sibling assertions in `tests/events.rs`. Unlike `events.jsonl`, `schema.json`
is hand-maintained — update it alongside any change to `docs/schema.md` or to
an event's shape in `src/events.rs`, and let the test catch drift.
