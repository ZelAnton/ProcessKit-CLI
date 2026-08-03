"""Offline tests for the cross-version interop lane's verdict logic.

The lane itself only runs on a schedule and needs two real binaries, so the part
that decides *whether a difference is a defect* — the two directional schema
relaxations and the event-shape drift classifier — would otherwise never be
exercised on a pull request. These tests pin that logic against both synthetic
schemas and this repository's own published fixtures, so a false pass or a false
failure in the scheduled lane shows up here first.
"""

import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))

import cross_version_interop as interop  # noqa: E402 - path set up above

try:
    import jsonschema
except ImportError:  # pragma: no cover - reported as a skip below
    jsonschema = None

REPOSITORY = Path(__file__).parents[2]
SCHEMA_DIR = REPOSITORY / "fixtures" / "schema"


def strict(properties, required, **extra):
    schema = {
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": False,
    }
    schema.update(extra)
    return schema


class RelaxationTests(unittest.TestCase):
    def test_upgrade_read_drops_required_but_keeps_the_closed_field_set(self):
        schema = strict({"a": {"type": "string"}}, ["a"])
        relaxed = interop.relax_for_upgrade_read(schema)
        self.assertNotIn("required", relaxed)
        self.assertIs(relaxed["additionalProperties"], False)
        self.assertEqual(schema["required"], ["a"], "the input must not be mutated")

    def test_downgrade_read_drops_the_closed_field_set_but_keeps_required(self):
        schema = strict({"a": {"type": "string"}}, ["a"])
        relaxed = interop.relax_for_downgrade_read(schema)
        self.assertNotIn("additionalProperties", relaxed)
        self.assertEqual(relaxed["required"], ["a"])

    def test_a_property_named_required_is_not_mistaken_for_the_keyword(self):
        schema = strict({"required": {"type": "boolean"}}, ["required"])
        relaxed = interop.relax_for_upgrade_read(schema)
        self.assertIn("required", relaxed["properties"])
        self.assertNotIn("required", relaxed)

    def test_nested_definitions_are_relaxed_too(self):
        schema = {"$defs": {"inner": strict({"a": {"type": "string"}}, ["a"])}}
        relaxed = interop.relax_for_upgrade_read(schema)
        self.assertNotIn("required", relaxed["$defs"]["inner"])

    def test_one_of_becomes_any_of_so_a_relaxed_branch_cannot_double_match(self):
        # `prune.schema.json` in miniature: relaxing `required` makes the tally form
        # match the dry-run branch as well, which `oneOf` would report as an error.
        document = {
            "oneOf": [
                {"$ref": "#/$defs/tally"},
                {"$ref": "#/$defs/preview"},
            ],
            "$defs": {
                "tally": strict({"pruned": {"type": "integer"}}, ["pruned"]),
                "preview": strict(
                    {"pruned": {"type": "integer"}, "candidates": {"type": "array"}},
                    ["pruned", "candidates"],
                ),
            },
        }
        relaxed = interop.relax_for_upgrade_read(document)
        self.assertNotIn("oneOf", relaxed)
        self.assertEqual(len(relaxed["anyOf"]), 2)
        if jsonschema is None:
            self.skipTest("jsonschema is not installed")
        self.assertEqual(interop.validate(relaxed, {"pruned": 1}), [])
        # The same relaxation left under `oneOf` reports the tally as invalid
        # because it now matches both branches — the false negative being avoided.
        still_one_of = dict(relaxed)
        still_one_of["oneOf"] = still_one_of.pop("anyOf")
        self.assertNotEqual(interop.validate(still_one_of, {"pruned": 1}), [])


class DirectionalVerdictTests(unittest.TestCase):
    """The two relaxations must call additive drift benign and removal breaking."""

    def setUp(self):
        if jsonschema is None:
            self.skipTest("jsonschema is not installed")
        self.old = strict({"event": {"const": "x"}, "a": {"type": "string"}},
                          ["event", "a"])
        # The current build added `b` and kept everything else.
        self.new = strict(
            {"event": {"const": "x"}, "a": {"type": "string"}, "b": {"type": "integer"}},
            ["event", "a", "b"],
        )

    def test_an_added_field_does_not_condemn_the_older_producer(self):
        old_line = {"event": "x", "a": "value"}
        self.assertNotEqual(interop.validate(self.new, old_line), [],
                            "the unrelaxed schema requires the added field")
        self.assertEqual(
            interop.validate(interop.relax_for_upgrade_read(self.new), old_line), []
        )

    def test_an_added_field_does_not_condemn_the_newer_producer_either(self):
        new_line = {"event": "x", "a": "value", "b": 1}
        self.assertNotEqual(interop.validate(self.old, new_line), [],
                            "the unrelaxed schema forbids the added field")
        self.assertEqual(
            interop.validate(interop.relax_for_downgrade_read(self.old), new_line), []
        )

    def test_a_removed_field_is_still_caught_in_the_upgrade_direction(self):
        shrunk = strict({"event": {"const": "x"}}, ["event"])
        old_line = {"event": "x", "a": "value"}
        self.assertNotEqual(
            interop.validate(interop.relax_for_upgrade_read(shrunk), old_line), []
        )

    def test_a_removed_field_is_still_caught_in_the_downgrade_direction(self):
        new_line = {"event": "x"}
        self.assertNotEqual(
            interop.validate(interop.relax_for_downgrade_read(self.old), new_line), []
        )

    def test_a_retyped_field_is_caught_in_both_directions(self):
        retyped = strict({"event": {"const": "x"}, "a": {"type": "integer"}},
                         ["event", "a"])
        self.assertNotEqual(
            interop.validate(interop.relax_for_upgrade_read(retyped),
                             {"event": "x", "a": "value"}), []
        )
        self.assertNotEqual(
            interop.validate(interop.relax_for_downgrade_read(self.old),
                             {"event": "x", "a": 1}), []
        )


class EventShapeTests(unittest.TestCase):
    def _schema(self, *events):
        return {
            "$defs": {
                f"def{index}": {"properties": dict(properties,
                                                   event={"const": tag})}
                for index, (tag, properties) in enumerate(events)
            }
        }

    def test_shapes_are_read_from_the_event_const_tag(self):
        schema = self._schema(("run_started", {"run_id": {}}))
        self.assertEqual(interop.event_shapes(schema),
                         {"run_started": {"event", "run_id"}})

    def test_a_definition_without_an_event_tag_is_ignored(self):
        schema = {"$defs": {"member": {"properties": {"pid": {}}}}}
        self.assertEqual(interop.event_shapes(schema), {})

    def test_added_events_and_fields_are_classified_as_additive(self):
        old = self._schema(("a", {"x": {}}))
        new = self._schema(("a", {"x": {}, "y": {}}), ("b", {}))
        drift = interop.compare_event_shapes(old, new)
        self.assertEqual(drift.added_events, ["b"])
        self.assertEqual(drift.added_fields, ["a.y"])
        self.assertEqual(drift.breaking, [])

    def test_removed_events_and_fields_are_classified_as_breaking(self):
        old = self._schema(("a", {"x": {}, "y": {}}), ("b", {}))
        new = self._schema(("a", {"x": {}}))
        drift = interop.compare_event_shapes(old, new)
        self.assertEqual(drift.removed_events, ["b"])
        self.assertEqual(drift.removed_fields, ["a.y"])
        self.assertEqual(drift.breaking, ["b", "a.y"])


class ScenarioStatusTests(unittest.TestCase):
    """A declared break must be visible without turning the lane red."""

    def scenario(self):
        return interop.Scenario(name="x", summary="y")

    def test_a_declared_break_alone_does_not_fail_the_scenario(self):
        scenario = self.scenario()
        scenario.warn("snapshot_version bumped")
        self.assertEqual(scenario.status, "warn")

    def test_a_failure_is_never_downgraded_by_a_later_declared_break(self):
        scenario = self.scenario()
        scenario.fail("a field vanished")
        scenario.warn("snapshot_version bumped")
        self.assertEqual(scenario.status, "fail")

    def test_a_failure_after_a_declared_break_still_wins(self):
        scenario = self.scenario()
        scenario.warn("snapshot_version bumped")
        scenario.fail("a field vanished")
        self.assertEqual(scenario.status, "fail")

    def test_require_records_the_failure_before_abandoning_the_scenario(self):
        scenario = self.scenario()
        with self.assertRaises(interop.Failed):
            scenario.require(False, "boom")
        self.assertEqual(scenario.status, "fail")
        self.assertEqual(scenario.failures, ["boom"])

    def test_a_skip_carries_its_reason(self):
        scenario = self.scenario()
        with self.assertRaises(interop.Skipped) as raised:
            scenario.skip("the old binary predates this flag")
        self.assertEqual(str(raised.exception), "the old binary predates this flag")


class SchemaVersionBumpTests(unittest.TestCase):
    def context(self, old_version, new_version):
        context = interop.Context.__new__(interop.Context)
        context.old_probe = {"schema_version": old_version}
        context.new_probe = {"schema_version": new_version}
        return context

    def test_an_equal_schema_version_is_not_a_bump(self):
        self.assertFalse(self.context(1, 1).schema_version_bumped)

    def test_a_differing_schema_version_is_a_bump(self):
        self.assertTrue(self.context(1, 2).schema_version_bumped)

    def test_an_unreported_schema_version_is_not_treated_as_a_bump(self):
        self.assertFalse(self.context(None, 2).schema_version_bumped)


class ProbeReportHelperTests(unittest.TestCase):
    def test_missing_surface_preserves_the_requested_order(self):
        report = {"surface": ["run", "run:--jsonl"]}
        self.assertEqual(
            interop.missing_surface(report, ["run:--detach", "run", "wait"]),
            ["run:--detach", "wait"],
        )

    def test_band_argument_renders_the_reserved_band(self):
        self.assertEqual(
            interop.band_argument({"exit_code_band": {"start": 100, "end": 119}}),
            "100-119",
        )

    def test_band_argument_refuses_a_malformed_report(self):
        self.assertIsNone(interop.band_argument({}))
        self.assertIsNone(interop.band_argument({"exit_code_band": {"start": 100}}))


class PointerTests(unittest.TestCase):
    def test_resolve_pointer_walks_objects_and_arrays(self):
        document = {"$defs": {"a": {"items": [{"const": 7}]}}}
        self.assertEqual(interop.resolve_pointer(document, "/$defs/a/items/0/const"), 7)

    def test_resolve_pointer_returns_none_for_an_absent_step(self):
        self.assertIsNone(interop.resolve_pointer({"$defs": {}}, "/$defs/missing/const"))

    def test_without_pointer_removes_only_the_named_value(self):
        document = {"$defs": {"a": {"properties": {"v": {"const": 2, "type": "integer"}}}}}
        stripped = interop.without_pointer(document, "/$defs/a/properties/v/const")
        self.assertNotIn("const", stripped["$defs"]["a"]["properties"]["v"])
        self.assertEqual(stripped["$defs"]["a"]["properties"]["v"]["type"], "integer")
        self.assertEqual(document["$defs"]["a"]["properties"]["v"]["const"], 2,
                         "the input document must not be mutated")

    def test_without_pointer_tolerates_an_absent_path(self):
        document = {"$defs": {}}
        self.assertEqual(interop.without_pointer(document, "/$defs/a/const"), document)

    def test_rooted_at_keeps_defs_resolvable(self):
        document = {"$schema": "https://json-schema.org/draft/2020-12/schema",
                    "$id": "https://example.invalid/x.json",
                    "oneOf": [{"$ref": "#/$defs/a"}],
                    "$defs": {"a": {"type": "integer"}}}
        rooted = interop.rooted_at(document, "/$defs/a")
        self.assertEqual(rooted["$ref"], "#/$defs/a")
        self.assertIn("$defs", rooted)
        self.assertNotIn("$id", rooted, "the base URI must not be inherited")
        if jsonschema is None:
            self.skipTest("jsonschema is not installed")
        self.assertEqual(interop.validate(rooted, 1), [])
        self.assertNotEqual(interop.validate(rooted, "1"), [])


class PublishedFixtureTests(unittest.TestCase):
    """Pin the driver against this repository's own published schema documents."""

    def setUp(self):
        if jsonschema is None:
            self.skipTest("jsonschema is not installed")

    def test_the_golden_event_stream_survives_both_relaxations(self):
        schema = json.loads((SCHEMA_DIR / "v1" / "schema.json").read_text(encoding="utf-8"))
        upgrade = interop.relax_for_upgrade_read(schema)
        downgrade = interop.relax_for_downgrade_read(schema)
        lines = interop.stream_lines(SCHEMA_DIR / "v1" / "events.jsonl")
        self.assertTrue(lines, "the golden stream fixture is empty")
        for index, line in lines:
            self.assertEqual(interop.validate(schema, line), [], f"line {index}")
            self.assertEqual(interop.validate(upgrade, line), [], f"line {index}")
            self.assertEqual(interop.validate(downgrade, line), [], f"line {index}")

    def test_every_published_event_type_is_discoverable_by_its_tag(self):
        schema = json.loads((SCHEMA_DIR / "v1" / "schema.json").read_text(encoding="utf-8"))
        shapes = interop.event_shapes(schema)
        branches = len(schema["oneOf"])
        self.assertEqual(
            len(shapes), branches,
            "every root branch must be reachable by its `event` const tag, or the"
            " drift classifier would silently ignore one",
        )
        for tag, properties in shapes.items():
            self.assertIn("schema_version", properties, tag)

    def test_a_schema_compared_with_itself_reports_no_drift(self):
        schema = json.loads((SCHEMA_DIR / "v1" / "schema.json").read_text(encoding="utf-8"))
        drift = interop.compare_event_shapes(schema, schema)
        self.assertEqual(drift.breaking, [])
        self.assertEqual(drift.added_events, [])
        self.assertEqual(drift.added_fields, [])

    def test_the_versioned_output_pointers_still_resolve(self):
        # docs/compatibility.md pins exactly two of the six published CLI-output
        # families on a version field of their own. If a document moves that field,
        # the scheduled lane must not quietly stop checking it.
        for family, field_name, pointer in interop.VERSIONED_CLI_OUTPUTS:
            document = json.loads(
                (SCHEMA_DIR / "cli" / f"{family}.schema.json").read_text(encoding="utf-8")
            )
            self.assertIsInstance(
                interop.resolve_pointer(document, pointer), int,
                f"{family}.schema.json no longer pins {field_name} at {pointer}",
            )

    def test_the_golden_cli_outputs_validate_under_the_upgrade_relaxation(self):
        for document_path in sorted((SCHEMA_DIR / "cli").glob("*.schema.json")):
            family = document_path.name[: -len(".schema.json")]
            document = json.loads(document_path.read_text(encoding="utf-8"))
            relaxed = interop.relax_for_upgrade_read(document)
            fixture = SCHEMA_DIR / "cli" / f"{family}.jsonl"
            for index, payload in enumerate(
                fixture.read_text(encoding="utf-8").splitlines(), start=1
            ):
                if payload.strip():
                    self.assertEqual(
                        interop.validate(relaxed, json.loads(payload)), [],
                        f"{fixture.name}:{index}",
                    )


if __name__ == "__main__":
    unittest.main()
