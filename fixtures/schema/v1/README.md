# JSONL schema v1 — golden fixtures

`events.jsonl` is the **golden sample stream** for schema version 1 of
processkit-cli's JSONL lifecycle-event contract: one representative instance of
every event type, in lifecycle order, serialized exactly as the runner writes
each line to a `--jsonl` file. Adapters (for example the processkit-py CLI) that
pin `schema_version` can use it as the reference material to build and test their
readers against.

The normative field-by-field description lives in
[`docs/schema.md`](../../../docs/schema.md). This fixture is the machine-readable
companion: the golden test in `src/events.rs`
(`events::tests::golden_stream_matches_the_fixture`) renders the same sample
events and asserts they match this file byte-for-byte, so any accidental change to
an event's wire shape fails the build.

- **Do not hand-edit** these lines. They are generated. Regenerate after an
  intentional, version-bumped schema change with:

  ```sh
  UPDATE_SCHEMA_GOLDEN=1 cargo test --bin processkit-cli \
      events::tests::golden_stream_matches_the_fixture
  ```

- The timestamps, run id, and PIDs here are fixed sample values chosen for a
  stable fixture; a real stream carries the actual run's values.
- A breaking change to any shape below is a **major** bump of `schema_version`
  (see `docs/schema.md`, "Versioning"), which lands under a new `fixtures/schema/vN/`.
