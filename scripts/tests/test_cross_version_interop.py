"""Offline tests for the cross-version interop lane's verdict logic.

The lane itself only runs on a schedule and needs two real binaries, so the part
that decides *whether a difference is a defect* — the two directional schema
relaxations, the event-shape drift classifier, and the version pins the driver
reads out of the published documents — would otherwise never be exercised on a
pull request. These tests pin that logic against both synthetic schemas and this
repository's own published fixtures, so a false pass or a false failure in the
scheduled lane shows up here first.
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

    def test_without_pointer_reports_nothing_when_the_step_is_absent(self):
        # The building block is deliberately forgiving, which is why every caller
        # decides *whether* to strip from a pin it actually resolved first: a
        # silent no-op here is how a document that moved a node would quietly stop
        # being excluded from the shape comparison.
        document = {"$defs": {"a": {"properties": {"v": {"enum": [1, 2]}}}}}
        self.assertEqual(interop.without_pointer(document, "/$defs/a/properties/v/const"),
                         document)

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


class VersionPinTests(unittest.TestCase):
    """Both published pin forms must answer the two questions this lane asks.

    `probe.schema.json` pins `probe_version` with `const`, while
    `inspect.schema.json` enumerates the *range* of `snapshot_version` values this
    build renders, because its client genuinely decodes an older runner's shape.
    The form follows the reader's tolerance rather than where the number comes
    from — `attest.schema.json`'s `attestation_version` is supplied by the runner
    too and is pinned with `const`, because its client refuses every version but
    its own (`fixtures/schema/cli/README.md`, "The `snapshot_version` range").
    The driver has to read either form: hard-coding one
    keyword makes a documented change of *form* read as the version field having
    disappeared, which both blinds the version check and downgrades every genuine
    shape defect in that family to a declared break.
    """

    def test_a_const_pin_is_read_as_its_single_value(self):
        pin = interop.version_pin({"const": 2, "description": "x"})
        self.assertEqual(pin.keyword, "const")
        self.assertEqual(pin.accepted, (2,))
        self.assertEqual(pin.render(), "2")

    def test_an_enumerated_range_is_read_as_every_value_it_admits(self):
        pin = interop.version_pin({"enum": [1, 2], "description": "x"})
        self.assertEqual(pin.keyword, "enum")
        self.assertEqual(pin.accepted, (1, 2))
        self.assertEqual(pin.render(), "one of 1, 2")

    def test_a_const_pin_admits_only_its_own_value(self):
        pin = interop.version_pin({"const": 1})
        self.assertTrue(pin.admits(1))
        self.assertFalse(pin.admits(2), "a bump away from a const pin is a real break")

    def test_a_range_admits_an_older_runners_version_but_not_an_unpublished_one(self):
        # The `bumped` verdict is `not admits(observed)`. Plain inequality against
        # one value would call a released runner's snapshot_version 1 a bump here,
        # even though this build renders it — the false negative that silently turns
        # every shape failure in the family into a tolerated declared break.
        pin = interop.version_pin({"enum": [1, 2]})
        self.assertTrue(pin.admits(1), "an older runner's snapshot is still rendered")
        self.assertTrue(pin.admits(2))
        self.assertFalse(pin.admits(3), "a version above the range is refused, not rendered")
        self.assertFalse(pin.admits(0), "a version below the floor is refused too")

    def test_a_boolean_is_not_a_version_though_python_equates_it_with_one(self):
        self.assertFalse(interop.version_pin({"const": 1}).admits(True))
        self.assertFalse(interop.version_pin({"enum": [1, 2]}).admits(True))

    def test_a_field_that_pins_nothing_is_reported_as_unpinned(self):
        # The one case that really does mean the checked versioned-output scope
        # changed: the node is gone, or it no longer constrains the field to a set
        # of versions.
        self.assertIsNone(interop.version_pin(None))
        self.assertIsNone(interop.version_pin({"type": "integer"}))
        self.assertIsNone(interop.version_pin({"enum": []}))
        self.assertIsNone(interop.version_pin({"enum": ["1", "2"]}))

    def test_without_version_pin_lifts_whichever_keyword_the_document_uses(self):
        for node in ({"const": 2, "description": "x"}, {"enum": [1, 2], "description": "x"}):
            with self.subTest(pin=node):
                document = {"$defs": {"a": {"properties": {"v": dict(node)}}}}
                stripped = interop.without_version_pin(document, "/$defs/a/properties/v")
                remaining = stripped["$defs"]["a"]["properties"]["v"]
                self.assertEqual(
                    remaining, {"description": "x"},
                    "only the pin is lifted — the property itself must survive, or"
                    " `additionalProperties: false` would reject the very field being"
                    " excluded and manufacture a defect",
                )
                self.assertIsNone(interop.version_pin(remaining))
                self.assertEqual(document["$defs"]["a"]["properties"]["v"], node,
                                 "the input document must not be mutated")


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

    def _pin(self, family, pointer):
        document = json.loads(
            (SCHEMA_DIR / "cli" / f"{family}.schema.json").read_text(encoding="utf-8")
        )
        return document, interop.version_pin(interop.resolve_pointer(document, pointer))

    def _first_fixture_line(self, family):
        lines = (SCHEMA_DIR / "cli" / f"{family}.jsonl").read_text(
            encoding="utf-8").splitlines()
        return json.loads(next(line for line in lines if line.strip()))

    def test_the_versioned_output_pins_still_resolve_in_whichever_form(self):
        # docs/compatibility.md pins four of the eight published CLI-output families
        # on a version field of their own; this lane compares a released binary, so
        # it checks the two of them a released binary emits (the `--error-format
        # json` envelope and `attest --json` are both new and unreleased — see
        # `VERSIONED_CLI_OUTPUTS`).
        # If a document moves that field,
        # the scheduled lane must not quietly stop checking it. The *form* of the
        # pin deliberately differs between the two documents (`const` for probe, an
        # enumerated range for inspect), so what has to resolve is a usable pin, not
        # one particular keyword: reading only `const` here is how the lane would go
        # red — and then blind — on a documented change to the other document.
        for family, field_name, pointer in interop.VERSIONED_CLI_OUTPUTS:
            with self.subTest(family=family):
                _, pin = self._pin(family, pointer)
                self.assertIsNotNone(
                    pin, f"{family}.schema.json no longer pins {field_name} at {pointer}"
                )
                self.assertIn(pin.keyword, interop.VERSION_PIN_KEYWORDS)
                self.assertTrue(pin.accepted, f"{family}.schema.json admits no version")
                for value in pin.accepted:
                    self.assertIsInstance(
                        value, int,
                        f"{family}.schema.json pins {field_name} at a non-version"
                        f" {value!r}",
                    )

    def test_a_published_pin_admits_exactly_the_versions_it_publishes(self):
        # Written against the pin itself rather than against today's numbers, so
        # moving either end of a range (which docs/compatibility.md licenses) does
        # not need this test edited, while the verdict semantics stay pinned.
        for family, _, pointer in interop.VERSIONED_CLI_OUTPUTS:
            with self.subTest(family=family):
                _, pin = self._pin(family, pointer)
                for value in pin.accepted:
                    self.assertTrue(pin.admits(value))
                self.assertFalse(pin.admits(max(pin.accepted) + 1))
                self.assertFalse(pin.admits(min(pin.accepted) - 1))

    def test_this_builds_own_output_is_never_read_as_a_version_bump(self):
        # The golden fixtures are the real binary's output. If a document stopped
        # admitting the value this build actually prints, the scheduled lane would
        # call it a declared break and tolerate every shape defect in that family.
        for family, field_name, pointer in interop.VERSIONED_CLI_OUTPUTS:
            with self.subTest(family=family):
                _, pin = self._pin(family, pointer)
                payload = self._first_fixture_line(family)
                self.assertIn(field_name, payload, f"{family}.jsonl carries no {field_name}")
                self.assertTrue(
                    pin.admits(payload[field_name]),
                    f"{family}.schema.json does not admit the {field_name}"
                    f" {payload[field_name]!r} that {family}.jsonl carries",
                )

    def test_the_version_pin_is_genuinely_lifted_out_of_the_shape_check(self):
        # The exclusion step must not degrade into a no-op when a document changes
        # the form of its pin: the version difference is reported once, by the
        # dedicated check, and the *rest* of the shape must still be compared. Both
        # halves are asserted against the real documents — the pin bites before it
        # is lifted, and only the pin is lifted.
        for family, field_name, pointer in interop.VERSIONED_CLI_OUTPUTS:
            with self.subTest(family=family):
                branch, separator, _ = pointer.rpartition("/properties/")
                self.assertTrue(separator, f"{pointer} does not name a property")
                document, pin = self._pin(family, pointer)
                unpublished = dict(self._first_fixture_line(family),
                                   **{field_name: max(pin.accepted) + 1})
                self.assertNotEqual(
                    interop.validate(
                        interop.relax_for_upgrade_read(interop.rooted_at(document, branch)),
                        unpublished),
                    [], f"{family}.schema.json does not constrain {field_name} at all",
                )
                stripped = interop.without_version_pin(document, pointer)
                self.assertIsNone(
                    interop.version_pin(interop.resolve_pointer(stripped, pointer)),
                    f"the {family} pin survived the exclusion step",
                )
                relaxed = interop.relax_for_upgrade_read(interop.rooted_at(stripped, branch))
                self.assertEqual(interop.validate(relaxed, unpublished), [],
                                 f"lifting the {family} pin left the version field checked")
                self.assertNotEqual(
                    interop.validate(relaxed, dict(unpublished, unexpected_field=1)), [],
                    f"lifting the {family} pin stopped the rest of the shape being checked",
                )
                self.assertIsNotNone(
                    interop.version_pin(interop.resolve_pointer(document, pointer)),
                    "the input document must not be mutated",
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
