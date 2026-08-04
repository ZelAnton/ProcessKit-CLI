#!/usr/bin/env python3
"""Execute docs/compatibility.md's mixed-version scenarios across two binaries.

The repository documents a mixed-binaries window in several places — the registry
record's own `registry_version`, the control plane's `snapshot_version`, the JSONL
stream's `schema_version`, and `probe`'s surface pinning — but every test tier
(unit, integration, e2e, stress) drives a single binary against itself. This
driver is the missing half: it points an *old* binary (the latest published
release) and a *new* binary (the current build) at one shared registry directory
and actually runs the upgrade/downgrade procedures from `docs/compatibility.md`.

It is invoked by `.github/workflows/interop.yml` and is runnable by hand:

    python scripts/cross_version_interop.py \\
        --old-binary /path/to/released/processkit-cli \\
        --new-binary target/release/processkit-cli \\
        --schema-dir fixtures/schema

Exit status: `0` when every scenario passed, declared a break, or was explicitly
skipped; `1` when a scenario failed; `2` on a harness error (bad arguments,
unusable binary). A skip always carries its reason — a scenario is never silently
dropped.

A *failure* is a compatibility break that no version field declares. A bump of one
of the three versioned contracts (`schema_version`, `snapshot_version`,
`probe_version`) is instead reported as a **declared break** (`Scenario.warn`):
bumping is the sanctioned way to break these, so failing on it would leave the
lane red from the moment a legitimate bump lands until the next release.

Two rules shape the assertions, both taken from `docs/compatibility.md`:

* **Upgrade direction** (an artifact produced by the *old* binary, read by the
  *new* one): a field the new build added since the release is additive and must
  not be reported as a defect, so a published `required` list is relaxed. A field
  the old binary emits that the new schema no longer permits *is* a defect.
* **Downgrade direction** (an artifact produced by the *new* binary, read by the
  *old* one): within one version a reader must tolerate new event types, new
  fields on an event it already parses (including always-present ones, not only
  optional ones), and unknown fields generally, so `additionalProperties` is
  relaxed and an event type unknown to the old schema is reported as additive. A
  field the old schema requires and the new binary no longer emits *is* a defect.

Both relaxations are pure functions over the schema documents
(`relax_for_upgrade_read` / `relax_for_downgrade_read`), as is the reading of a
published version pin (`version_pin`), and all of them are unit-tested offline by
`scripts/tests/test_cross_version_interop.py` against this repository's own
documents, so this lane's own verdict logic cannot rot unnoticed between
scheduled runs.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
from typing import Any, Callable, Iterable, Sequence

try:  # pragma: no cover - the workflow installs the package before running
    import jsonschema
except ImportError:  # pragma: no cover - keeps the script runnable without pip
    jsonschema = None


# Every CLI invocation this driver makes is bounded; a wedged binary must not sit
# until the job's own timeout with no diagnosis.
CLIENT_TIMEOUT_SEC = 60.0
# Each run this driver launches self-bounds too (`run --timeout`), so a cancelled
# job cannot leave a runner behind on the shared CI machine.
RUN_TIMEOUT = "60s"
# How long a scenario waits for a registry/liveness transition it just caused.
SETTLE_TIMEOUT_SEC = 45.0
POLL_INTERVAL_SEC = 0.2
# The idling payload's own ceiling, comfortably inside RUN_TIMEOUT.
CHILD_LIFETIME_SEC = "50"

# `docs/exit-codes.md`: a fail-closed preflight refusal.
EXIT_PROBE_INCOMPATIBLE = 110

# JSON Schema keywords, grouped by how their value nests further schemas. The
# relaxations below walk with this map rather than by guessing, so a *property*
# that happens to be named `required` or `oneOf` is never rewritten as if it were
# a keyword.
SCHEMA_VALUED = ("additionalProperties", "items", "not", "if", "then", "else",
                 "contains", "propertyNames", "unevaluatedProperties")
SCHEMA_LIST_VALUED = ("oneOf", "anyOf", "allOf", "prefixItems")
SCHEMA_MAP_VALUED = ("properties", "patternProperties", "$defs", "definitions",
                     "dependentSchemas")

# `docs/compatibility.md`, "Machine-output schemas": of the seven published
# `fixtures/schema/cli/` families, THREE carry a version field of their own —
# `probe --json` (`probe_version`), `inspect --json` (`snapshot_version`), and the
# `--error-format json` failure envelope (`error_version`). The other four (`list`,
# `control-ack`, `prune`, `wait`) deliberately do not and ride on the CLI surface
# instead. This lane compares a *released* binary against the current documents, so
# it checks the two of those three a released binary can actually be compared on:
# the envelope is new in the current release and no published binary emits it yet,
# leaving nothing on the other side of the comparison. Add it here the first time a
# release ships `--error-format`. Keep that scope explicit either way: it must not
# be generalised in either direction.
#
# Each entry addresses the version field's own schema NODE, not one pinning
# keyword inside it, because the two documents pin in two deliberately different
# forms: `probe.schema.json` pins `probe_version` with `const` (the invoked binary
# writes that value itself), while `inspect.schema.json` enumerates the *range* of
# `snapshot_version` values this build renders, because that number is supplied by
# the *runner* on the far side of the control-plane wire and a run started by an
# older build reports that build's number (`fixtures/schema/cli/README.md`, "The
# `snapshot_version` range"; `docs/compatibility.md`, "Machine-output schemas").
# `version_pin()` below reads whichever form a document publishes, so a pin that
# changes *shape* is followed rather than misread as the field having disappeared
# — the pointer must keep naming the field, and the driver must keep following the
# document, not the other way round.
VERSIONED_CLI_OUTPUTS = (
    ("probe", "probe_version", "/$defs/probeReport/properties/probe_version"),
    ("inspect", "snapshot_version", "/$defs/snapshot/properties/snapshot_version"),
)


# --------------------------------------------------------------------------- #
# Pure helpers (unit-tested offline)
# --------------------------------------------------------------------------- #


def _walk_schema(node: Any, transform: Callable[[dict], dict]) -> Any:
    """Apply `transform` to every schema object reachable from `node`.

    Recursion follows JSON Schema keyword structure only. Anything else (a
    `description` string, an `enum` list, a `const` object) is copied verbatim.
    """
    if not isinstance(node, dict):
        return node
    copied: dict[str, Any] = {}
    for key, value in node.items():
        if key in SCHEMA_VALUED and isinstance(value, dict):
            copied[key] = _walk_schema(value, transform)
        elif key in SCHEMA_LIST_VALUED and isinstance(value, list):
            copied[key] = [_walk_schema(item, transform) for item in value]
        elif key in SCHEMA_MAP_VALUED and isinstance(value, dict):
            copied[key] = {name: _walk_schema(item, transform)
                           for name, item in value.items()}
        else:
            copied[key] = value
    return transform(copied)


def _loosen_one_of(node: dict) -> dict:
    """Rewrite `oneOf` as `anyOf`.

    A relaxed branch can start matching a payload it used to exclude: dropping
    `required` from `prune.schema.json`'s two forms makes a plain tally match the
    dry-run branch as well. Under `oneOf` that second match is reported as a
    validation error — exactly the false negative this lane must not produce,
    since the compatibility question being asked is "is this payload still
    recognised by at least one published form".
    """
    if "oneOf" in node and "anyOf" not in node:
        node = dict(node)
        node["anyOf"] = node.pop("oneOf")
    return node


def relax_for_upgrade_read(schema: Any) -> Any:
    """Relax `schema` for reading an OLDER producer's payload.

    Drops every `required` list: a field the newer schema requires and the older
    producer never emitted is an additive change, which `docs/compatibility.md`
    permits inside one schema version. Everything that detects a *breaking*
    change is kept — `additionalProperties: false` still rejects a field the new
    schema no longer permits, and declared types still reject a retyped field.
    """

    def transform(node: dict) -> dict:
        node.pop("required", None)
        return _loosen_one_of(node)

    return _walk_schema(schema, transform)


def relax_for_downgrade_read(schema: Any) -> Any:
    """Relax `schema` for reading a NEWER producer's payload.

    Drops `additionalProperties: false`: within one schema version a reader must
    tolerate new fields on an event it already parses — always-present ones
    included, not only optional ones — plus unknown fields generally
    (`docs/compatibility.md`, "What a reader must tolerate within one version").
    `required` is deliberately kept — a field the older schema requires and the
    newer producer stopped emitting is a removal, which is breaking.
    """

    def transform(node: dict) -> dict:
        if node.get("additionalProperties") is False:
            node.pop("additionalProperties")
        return _loosen_one_of(node)

    return _walk_schema(schema, transform)


def event_shapes(schema: dict) -> dict[str, set[str]]:
    """Map each JSONL event tag in `schema` to the property names it declares.

    Every event branch of `fixtures/schema/v1/schema.json` lives under `$defs` and
    pins its own tag with `properties.event.const`, so this is a direct structural
    read of the published shape: no stream has to exercise an event for that
    event's shape to be compared across versions.
    """
    shapes: dict[str, set[str]] = {}
    for definition in (schema.get("$defs") or {}).values():
        if not isinstance(definition, dict):
            continue
        properties = definition.get("properties")
        if not isinstance(properties, dict):
            continue
        tag = properties.get("event")
        if not isinstance(tag, dict) or not isinstance(tag.get("const"), str):
            continue
        shapes[tag["const"]] = set(properties)
    return shapes


@dataclass
class ShapeDrift:
    """How a newer JSONL schema differs from an older one, split by severity."""

    removed_events: list[str] = field(default_factory=list)
    added_events: list[str] = field(default_factory=list)
    removed_fields: list[str] = field(default_factory=list)
    added_fields: list[str] = field(default_factory=list)

    @property
    def breaking(self) -> list[str]:
        return self.removed_events + self.removed_fields


def compare_event_shapes(old: dict, new: dict) -> ShapeDrift:
    """Classify old-vs-new JSONL schema drift as additive or breaking."""
    old_shapes = event_shapes(old)
    new_shapes = event_shapes(new)
    drift = ShapeDrift()
    drift.removed_events = sorted(set(old_shapes) - set(new_shapes))
    drift.added_events = sorted(set(new_shapes) - set(old_shapes))
    for tag in sorted(set(old_shapes) & set(new_shapes)):
        drift.removed_fields.extend(
            f"{tag}.{name}" for name in sorted(old_shapes[tag] - new_shapes[tag])
        )
        drift.added_fields.extend(
            f"{tag}.{name}" for name in sorted(new_shapes[tag] - old_shapes[tag])
        )
    return drift


def resolve_pointer(document: Any, pointer: str) -> Any:
    """Resolve a JSON Pointer, returning `None` when any step is absent."""
    node = document
    for raw in pointer.split("/"):
        if raw == "":
            continue
        token = raw.replace("~1", "/").replace("~0", "~")
        if isinstance(node, dict) and token in node:
            node = node[token]
        elif isinstance(node, list) and token.isdigit() and int(token) < len(node):
            node = node[int(token)]
        else:
            return None
    return node


def without_pointer(document: Any, pointer: str) -> Any:
    """`document` with the value at `pointer` removed, copying only along the path.

    The building block of `without_version_pin` below, which is what lifts a
    versioned family's pin out of the *shape* check.
    """
    steps = [raw.replace("~1", "/").replace("~0", "~")
             for raw in pointer.split("/") if raw != ""]
    if not steps:
        return document
    root = dict(document) if isinstance(document, dict) else document
    node = root
    for step in steps[:-1]:
        if not isinstance(node, dict) or step not in node:
            return root
        child = node[step]
        node[step] = dict(child) if isinstance(child, dict) else child
        node = node[step]
    if isinstance(node, dict):
        node.pop(steps[-1], None)
    return root


# The keywords a published document may pin a version field's value with, most
# specific first. `without_version_pin` strips every one of them, so a document
# that ever carried both would still be lifted cleanly out of the shape check.
VERSION_PIN_KEYWORDS = ("const", "enum")


@dataclass(frozen=True)
class VersionPin:
    """The set of values a published document admits for one version field.

    Both pin forms this repository uses collapse to the same two questions, which
    is why the driver reads a `VersionPin` rather than one keyword's raw value:

    * `admits(value)` — is a version a *reader* can be handed still published
      here? For `probe.schema.json`'s `const` there is exactly one such value; for
      `inspect.schema.json`'s enumerated range there are as many as this build
      renders (`MIN_READABLE_SNAPSHOT_VERSION..=SNAPSHOT_VERSION`).
    * "has this been bumped since the release" — which is `not admits(observed)`,
      *not* inequality: a released runner's `snapshot_version` 1 read by a build
      that writes 2 is inside the published range and is not a break, whereas
      inequality against a single resolved value would report one and so downgrade
      every genuine shape defect in that family to a declared break.
    """

    keyword: str
    accepted: tuple[Any, ...]

    def admits(self, value: Any) -> bool:
        """Whether the document still publishes `value` for this field.

        `bool` is rejected outright: Python makes `True == 1`, and a payload whose
        version field is a boolean is not a version any of these documents admit.
        """
        if not isinstance(value, int) or isinstance(value, bool):
            return False
        return value in self.accepted

    def render(self) -> str:
        """The pin as a failure message should name it."""
        if len(self.accepted) == 1:
            return str(self.accepted[0])
        return "one of " + ", ".join(str(value) for value in self.accepted)


def version_pin(node: Any) -> VersionPin | None:
    """Read a version field's pin from its schema `node`, in whichever form it uses.

    `None` means the family genuinely stopped pinning its own version — the node
    is absent, or it constrains the field to something that is not a set of
    version integers — as opposed to the pin merely having changed shape, which is
    a documented decision this driver follows (see `VERSIONED_CLI_OUTPUTS`).
    """
    if not isinstance(node, dict):
        return None
    if "const" in node:
        keyword, values = "const", (node["const"],)
    elif isinstance(node.get("enum"), list):
        keyword, values = "enum", tuple(node["enum"])
    else:
        return None
    if not values or not all(
        isinstance(value, int) and not isinstance(value, bool) for value in values
    ):
        return None
    return VersionPin(keyword=keyword, accepted=values)


def without_version_pin(document: Any, pointer: str) -> Any:
    """`document` with the version pin at `pointer` lifted out of the shape check.

    A `probe_version`/`snapshot_version` difference is one fact, and the dedicated
    check over `VERSIONED_CLI_OUTPUTS` owns reporting it. Leaving the pin in place
    would also fail the payload's shape validation, reporting the same difference a
    second time and burying any genuine field-level defect underneath it. Only the
    pinning keywords are removed, never the property itself: these documents set
    `additionalProperties: false`, so dropping the property would turn the version
    field into an unexpected one and manufacture the very defect being excluded.
    """
    for keyword in VERSION_PIN_KEYWORDS:
        document = without_pointer(document, f"{pointer}/{keyword}")
    return document


def rooted_at(document: dict, pointer: str) -> dict:
    """A schema document re-rooted at `pointer`, keeping `$defs` resolvable.

    Validating a known output form against its own named branch (rather than the
    document's root `oneOf`) keeps the error message pointed at the real problem
    instead of reporting that every branch failed.
    """
    rooted: dict[str, Any] = {"$ref": f"#{pointer}"}
    for key in ("$schema", "$defs"):
        if key in document:
            rooted[key] = document[key]
    return rooted


def missing_surface(report: dict, tokens: Iterable[str]) -> list[str]:
    """Which of `tokens` a `probe --json` report does not advertise."""
    available = set(report.get("surface") or [])
    return [token for token in tokens if token not in available]


def band_argument(report: dict) -> str | None:
    """A probe report's reserved exit-code band, as `--require-exit-code-band` wants it."""
    band = report.get("exit_code_band")
    if not isinstance(band, dict):
        return None
    start, end = band.get("start"), band.get("end")
    if not isinstance(start, int) or not isinstance(end, int):
        return None
    return f"{start}-{end}"


# --------------------------------------------------------------------------- #
# Scenario bookkeeping
# --------------------------------------------------------------------------- #


class Skipped(Exception):
    """Raised inside a scenario to end it as an explicitly-reported skip."""


class Failed(Exception):
    """Raised inside a scenario when continuing would only produce noise."""


@dataclass
class Scenario:
    name: str
    summary: str
    status: str = "pass"
    notes: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    failures: list[str] = field(default_factory=list)
    skip_reason: str = ""

    def note(self, message: str) -> None:
        self.notes.append(message)

    def warn(self, message: str) -> None:
        """Record a licensed break: visible, but not a lane failure.

        A change to one of the three versioned contracts (`schema_version`,
        `snapshot_version`, `probe_version`) is the *sanctioned* way to make a
        breaking change — the field exists so the break is detectable — and the
        binary that bumped it did the right thing. Failing here would leave the
        lane permanently red from the moment a legitimate bump lands until the
        next release, which is precisely the kind of noise that trains a reader to
        ignore a scheduled lane and so masks the defects it does catch.
        """
        if self.status == "pass":
            self.status = "warn"
        self.warnings.append(message)

    def fail(self, message: str) -> None:
        self.status = "fail"
        self.failures.append(message)

    def require(self, condition: bool, message: str) -> None:
        """Record a failure and abandon the scenario when `condition` is false."""
        if not condition:
            self.fail(message)
            raise Failed(message)

    def skip(self, reason: str) -> None:
        raise Skipped(reason)


@dataclass
class Invocation:
    argv: list[str]
    returncode: int
    stdout: str
    stderr: str

    def diagnostics(self) -> str:
        """The most useful single line of output, for a failure message.

        The *first* line, not the last: both this binary's own errors and clap's
        usage errors lead with the actual reason and trail off into "For more
        information, try '--help'", which diagnoses nothing.
        """
        for line in (self.stderr or self.stdout or "").splitlines():
            if line.strip():
                return line.strip()
        return "(no output)"

    def json_tail(self) -> Any:
        """The last stdout line parsed as JSON, or `None` when it is not JSON."""
        stripped = (self.stdout or "").strip()
        if not stripped:
            return None
        try:
            return json.loads(stripped.splitlines()[-1])
        except json.JSONDecodeError:
            return None


class Cli:
    """One binary under test, pinned to the shared scratch registry directory."""

    def __init__(self, label: str, path: Path, registry_dir: Path) -> None:
        self.label = label
        self.path = path
        self.registry_dir = registry_dir

    def environment(self) -> dict[str, str]:
        env = dict(os.environ)
        # Both binaries share ONE scratch registry — that shared directory is the
        # mixed-version window being tested — and nothing here ever touches the
        # machine's own per-user registry.
        env["PROCESSKIT_CLI_REGISTRY_DIR"] = str(self.registry_dir)
        return env

    def invoke(self, *args: str, timeout: float = CLIENT_TIMEOUT_SEC) -> Invocation:
        argv = [str(self.path), *args]
        try:
            completed = subprocess.run(
                argv,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                env=self.environment(),
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as expired:
            return Invocation(argv, -1, expired.stdout or "", f"timed out after {timeout}s")
        return Invocation(argv, completed.returncode, completed.stdout, completed.stderr)

    def spawn(self, *args: str) -> subprocess.Popen:
        """Start a foreground run whose *runner* process this driver owns.

        The stale-leftover scenarios need to kill the runner (not the child) to
        reproduce a record whose liveness lock nobody holds any more — precisely
        the abrupt-death leftover `prune` exists to reap. Owning the process is
        the portable way to do that on all three platforms.
        """
        return subprocess.Popen(
            [str(self.path), *args],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=self.environment(),
        )


def poll(predicate: Callable[[], Any], timeout: float = SETTLE_TIMEOUT_SEC) -> Any:
    """Poll `predicate` until it returns something truthy, or the deadline passes."""
    deadline = time.monotonic() + timeout
    while True:
        value = predicate()
        if value:
            return value
        if time.monotonic() >= deadline:
            return None
        time.sleep(POLL_INTERVAL_SEC)


# --------------------------------------------------------------------------- #
# Driver context
# --------------------------------------------------------------------------- #


@dataclass
class Context:
    old: Cli
    new: Cli
    work_dir: Path
    schema_dir: Path
    child_script: Path
    new_schema: dict
    archive_schema: Path | None = None
    old_probe: dict = field(default_factory=dict)
    new_probe: dict = field(default_factory=dict)
    old_schema: dict | None = None
    old_schema_origin: str = ""
    # Recorded JSONL streams per producing binary, replayed by the schema scenario.
    streams: dict[str, list[Path]] = field(
        default_factory=lambda: {"old": [], "new": []}
    )

    def scratch(self, name: str) -> Path:
        path = self.work_dir / name
        path.mkdir(parents=True, exist_ok=True)
        return path

    def reset_registry(self) -> None:
        """Start a scenario from an empty shared registry directory."""
        registry = self.old.registry_dir
        shutil.rmtree(registry, ignore_errors=True)
        registry.mkdir(parents=True, exist_ok=True)

    @property
    def schema_version_bumped(self) -> bool:
        """Whether the JSONL `schema_version` differs between the two binaries."""
        old = self.old_probe.get("schema_version")
        new = self.new_probe.get("schema_version")
        return old is not None and new is not None and old != new

    def supports(self, which: str, *tokens: str) -> bool:
        """Whether the `old`/`new` binary advertises every one of `tokens`."""
        report = self.old_probe if which == "old" else self.new_probe
        return not missing_surface(report, tokens)

    def require_surface(self, scenario: Scenario, **by_binary: Sequence[str]) -> None:
        """Skip `scenario` unless each binary advertises the tokens it must drive.

        Gating on each binary's own `probe --json` surface — rather than on an
        assumption about how old the published release is — is what lets this lane
        keep running against a release that predates a flag it would otherwise
        use, instead of failing for a reason that is not a compatibility defect.
        """
        reports = {"old": self.old_probe, "new": self.new_probe}
        for which, tokens in by_binary.items():
            absent = missing_surface(reports[which], tokens)
            if absent:
                scenario.skip(
                    f"the {which} binary does not expose {', '.join(absent)}"
                    " (tokens this scenario drives)"
                )


def entries(cli: Cli, scenario: Scenario) -> list[dict]:
    """`list --json` as parsed objects, failing the scenario on a bad exit."""
    result = cli.invoke("list", "--json")
    scenario.require(
        result.returncode == 0,
        f"`{cli.label} list --json` exited {result.returncode}: {result.diagnostics()}",
    )
    return [json.loads(line) for line in result.stdout.splitlines() if line.strip()]


def entry_for(cli: Cli, scenario: Scenario, run_id: str) -> dict | None:
    for entry in entries(cli, scenario):
        if entry.get("run_id") == run_id:
            return entry
    return None


def start_detached(ctx: Context, cli: Cli, scenario: Scenario, run_id: str,
                   events: Path, release: Path) -> bool:
    """Start a detached run that idles until `release` appears, and confirm it started.

    Returns whether the run carries an operator label. `run --label` is itself an
    additive flag, so a release old enough to predate it must still be able to
    launch the run every other assertion in the scenario depends on — passing the
    flag unconditionally would turn "this release is old" into a usage error (100)
    and report it as a compatibility failure.
    """
    labelled = ctx.supports(cli.label, "run:--label")
    label_args = ["--label", "lane=interop"] if labelled else []
    result = cli.invoke(
        "run", "--detach",
        "--run-id", run_id,
        *label_args,
        "--jsonl", str(events),
        "--timeout", RUN_TIMEOUT,
        "--", sys.executable, str(ctx.child_script), str(release), CHILD_LIFETIME_SEC,
    )
    scenario.require(
        result.returncode == 0,
        f"`{cli.label} run --detach` exited {result.returncode}: {result.diagnostics()}",
    )
    ctx.streams[cli.label].append(events)
    return labelled


def start_and_abandon(ctx: Context, cli: Cli, scenario: Scenario, run_id: str,
                      events: Path, release: Path) -> None:
    """Leave behind the registry record of a runner that died abruptly.

    A foreground run is started with this driver as its parent, waited for until it
    has registered, then hard-killed. The record survives; its liveness lock does
    not, because the kernel releases it when the process dies — the exact shape of
    leftover that `prune` classifies as reapable.
    """
    process = cli.spawn(
        "run",
        "--run-id", run_id,
        "--jsonl", str(events),
        "--timeout", RUN_TIMEOUT,
        "--", sys.executable, str(ctx.child_script), str(release), CHILD_LIFETIME_SEC,
    )
    try:
        registered = poll(lambda: entry_for(cli, scenario, run_id) is not None)
        scenario.require(
            bool(registered),
            f"the {cli.label} binary never registered `{run_id}` in the shared registry",
        )
    finally:
        process.kill()
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:  # pragma: no cover - defensive
            pass
    ctx.streams[cli.label].append(events)


def produce_teardown_stream(ctx: Context, cli: Cli, events: Path, release: Path) -> bool:
    """Record a multi-event JSONL stream from `cli` via a deliberate timeout.

    A run torn down by its own `--timeout` emits lifecycle events a fast-exiting
    child never does (`timeout`, `cleanup_started`, `cleanup_finished`), so the
    schema scenario gets a stream worth cross-reading even when the control-plane
    scenarios above were skipped.
    """
    # A timed-out run reports the reserved TIMEOUT code, not success; the stream is
    # what matters here, so only a missing or empty file is a problem.
    cli.invoke(
        "run",
        "--jsonl", str(events),
        "--timeout", "2s",
        "--", sys.executable, str(ctx.child_script), str(release), "30",
        timeout=120.0,
    )
    return events.is_file() and events.stat().st_size > 0


def load_old_schema(ctx: Context) -> None:
    """Obtain the released binary's own JSONL schema document, if it publishes one."""
    if ctx.old_schema is not None or ctx.old_schema_origin:
        return
    if missing_surface(ctx.old_probe, ["probe", "probe:--print-schema"]):
        ctx.old_schema_origin = "the released binary predates `probe --print-schema`"
    else:
        printed = ctx.old.invoke("probe", "--json", "--print-schema")
        if printed.returncode == 0 and printed.stdout.strip():
            ctx.old_schema = json.loads(printed.stdout)
            ctx.old_schema_origin = "the released binary's `probe --print-schema`"
            return
        ctx.old_schema_origin = f"`probe --print-schema` exited {printed.returncode}"
    # The release archives ship a `schema/schema.json` snapshot next to the binary
    # (release.yml's `build-artifacts` staging), so a release that predates
    # `--print-schema` can still be compared against.
    if ctx.archive_schema is not None and ctx.archive_schema.is_file():
        ctx.old_schema = json.loads(ctx.archive_schema.read_text(encoding="utf-8"))
        ctx.old_schema_origin = f"the release archive's {ctx.archive_schema.name}"


# --------------------------------------------------------------------------- #
# Scenarios
# --------------------------------------------------------------------------- #


def scenario_probe_reports(ctx: Context, scenario: Scenario) -> None:
    """Both binaries must produce a usable preflight report before anything else runs."""
    for which, cli in (("old", ctx.old), ("new", ctx.new)):
        result = cli.invoke("probe", "--json")
        scenario.require(
            result.returncode == 0,
            f"`{which} probe --json` exited {result.returncode}: {result.diagnostics()}",
        )
        report = result.json_tail()
        scenario.require(
            isinstance(report, dict),
            f"`{which} probe --json` did not print a JSON report",
        )
        if which == "old":
            ctx.old_probe = report
        else:
            ctx.new_probe = report
        scenario.note(
            f"{which}: version {report.get('version')},"
            f" schema_version {report.get('schema_version')},"
            f" probe_version {report.get('probe_version')},"
            f" band {band_argument(report)},"
            f" {len(report.get('surface') or [])} surface tokens"
        )
    old_only = sorted(set(ctx.old_probe.get("surface") or [])
                      - set(ctx.new_probe.get("surface") or []))
    new_only = sorted(set(ctx.new_probe.get("surface") or [])
                      - set(ctx.old_probe.get("surface") or []))
    scenario.note(f"surface added since the release: {', '.join(new_only) or 'none'}")
    scenario.note(f"surface withdrawn since the release: {', '.join(old_only) or 'none'}")


def scenario_probe_pinning_upgrade(ctx: Context, scenario: Scenario) -> None:
    """The current build must still satisfy the released binary's own pinning.

    The upgrade half of `docs/compatibility.md`'s "Fail-closed preflight": an
    adapter pinned against the published release must be able to point the same
    requirements at the new build and be told it is compatible.
    """
    band = band_argument(ctx.old_probe)
    tokens = sorted(ctx.old_probe.get("surface") or [])
    scenario.require(band is not None, "the released binary reported no exit-code band")
    scenario.require(bool(tokens), "the released binary reported no surface tokens")

    axes = (
        ("schema version",
         ["--require-schema-version", str(ctx.old_probe.get("schema_version"))]),
        ("exit-code band", ["--require-exit-code-band", str(band)]),
        ("surface", [arg for token in tokens for arg in ("--require-surface", token)]),
    )
    for axis, args in axes:
        result = ctx.new.invoke("probe", "--json", *args)
        if result.returncode == 0:
            scenario.note(f"the current build still satisfies the release's {axis} pinning")
            continue
        report = result.json_tail()
        mismatches = report.get("mismatches") if isinstance(report, dict) else None
        detail = "; ".join(mismatches) if mismatches else result.diagnostics()
        if axis == "schema version" and ctx.schema_version_bumped:
            scenario.warn(
                "the JSONL schema_version was bumped from"
                f" {ctx.old_probe.get('schema_version')} to"
                f" {ctx.new_probe.get('schema_version')} since the release, so an"
                " adapter pinned to the old value is refused by design. This must"
                " ship as a breaking release, with the new stream published under"
                " its own fixtures/schema/vN/ directory"
            )
            continue
        scenario.fail(
            f"the current build no longer satisfies the released binary's {axis} pinning"
            f" (exit {result.returncode}): {detail}."
            " An adapter pinned against the published release would be refused by the"
            " next one, and no version field declares the change"
        )


def scenario_probe_pinning_downgrade(ctx: Context, scenario: Scenario) -> None:
    """The released binary must refuse — fail-closed — what it cannot provide.

    The downgrade half. Requiring the current build's full token set from the old
    binary must either succeed (nothing was added since the release) or be refused
    with `PROBE_INCOMPATIBLE` (110). Exiting `0` while lacking a token is the
    false-OK the preflight exists to prevent, so both outcomes are asserted
    against what the two surfaces actually differ by.
    """
    old_tokens = sorted(ctx.old_probe.get("surface") or [])
    new_tokens = sorted(ctx.new_probe.get("surface") or [])
    absent = [token for token in new_tokens if token not in set(old_tokens)]

    # Positive control first: a token the release genuinely has must be accepted,
    # so a blanket-refusing probe cannot satisfy the negative control by accident.
    held = [arg for token in old_tokens for arg in ("--require-surface", token)]
    accepted = ctx.old.invoke("probe", "--json", *held)
    if accepted.returncode != 0:
        scenario.fail(
            "the released binary refused its own advertised surface"
            f" (exit {accepted.returncode}): {accepted.diagnostics()}"
        )

    requested = [arg for token in new_tokens for arg in ("--require-surface", token)]
    refused = ctx.old.invoke("probe", "--json", *requested)
    report = refused.json_tail()
    if not absent:
        if refused.returncode != 0:
            scenario.fail(
                "the current build advertises no new token, yet the released binary"
                f" refused its surface (exit {refused.returncode}):"
                f" {refused.diagnostics()}"
            )
        else:
            scenario.note("no surface token was added since the release")
    elif refused.returncode != EXIT_PROBE_INCOMPATIBLE:
        scenario.fail(
            "the released binary did not fail closed on the current build's surface:"
            f" expected exit {EXIT_PROBE_INCOMPATIBLE}, got {refused.returncode}."
            f" Tokens it does not expose: {', '.join(absent)}"
        )
    elif not isinstance(report, dict) or report.get("compatible") is not False \
            or not report.get("mismatches"):
        scenario.fail(
            "the released binary exited PROBE_INCOMPATIBLE but its report does not mark"
            f" itself incompatible or names no mismatch: {report!r}"
        )
    else:
        scenario.note(
            f"the released binary refuses the {len(absent)} token(s) added since it"
            " shipped, fail-closed with PROBE_INCOMPATIBLE (110)"
        )

    # The other two pinning axes, in the same direction.
    unchanged_schema = (ctx.new_probe.get("schema_version")
                        == ctx.old_probe.get("schema_version"))
    unchanged_band = band_argument(ctx.new_probe) == band_argument(ctx.old_probe)
    for axis, args, unchanged in (
        ("schema version",
         ["--require-schema-version", str(ctx.new_probe.get("schema_version"))],
         unchanged_schema),
        ("exit-code band",
         ["--require-exit-code-band", str(band_argument(ctx.new_probe))],
         unchanged_band),
    ):
        result = ctx.old.invoke("probe", "--json", *args)
        if unchanged and result.returncode != 0:
            scenario.fail(
                f"the {axis} is unchanged since the release, yet the released binary"
                f" refused it (exit {result.returncode}): {result.diagnostics()}"
            )
        elif not unchanged and result.returncode != EXIT_PROBE_INCOMPATIBLE:
            scenario.fail(
                f"the {axis} changed since the release, so the released binary had to"
                f" fail closed with {EXIT_PROBE_INCOMPATIBLE}; it exited"
                f" {result.returncode} instead"
            )
        elif not unchanged:
            scenario.note(f"the {axis} changed since the release and is refused fail-closed")


def scenario_new_clients_over_old_runner(ctx: Context, scenario: Scenario) -> None:
    """Current `list`/`inspect`/`prune`/`cancel`/`wait` over a released binary's live run."""
    ctx.require_surface(
        scenario,
        old=["run", "run:--detach", "run:--jsonl", "run:--run-id", "run:--timeout"],
        new=["list", "list:--json", "inspect", "inspect:--run-id", "inspect:--json",
             "prune", "prune:--json", "prune:--dry-run", "cancel", "cancel:--run-id",
             "wait", "wait:--run-id", "wait:--timeout"],
    )
    ctx.reset_registry()
    workspace = ctx.scratch("old-runner")
    run_id = "interop-old-runner"
    release = workspace / "release-old-runner"
    labelled = start_detached(ctx, ctx.old, scenario, run_id,
                              workspace / "old-runner.jsonl", release)

    try:
        entry = poll(lambda: entry_for(ctx.new, scenario, run_id))
        scenario.require(
            entry is not None,
            "the current `list --json` cannot see the run the released binary"
            " registered — a record the current build refuses to read is exactly the"
            " mid-upgrade blindness the registry's own version axis exists to prevent",
        )
        scenario.require(
            entry.get("health") == "live",
            f"the current `list --json` reports health {entry.get('health')!r} for a run"
            " the released binary has live",
        )
        if not labelled:
            scenario.note("the released binary predates `run --label`; labels not exercised")
        elif entry.get("labels") != {"lane": "interop"}:
            scenario.fail(
                "the current `list --json` lost the labels the released binary recorded:"
                f" {entry.get('labels')!r}"
            )
        scenario.note("current `list --json` reads the released binary's live record")

        snapshot = ctx.new.invoke("inspect", "--run-id", run_id, "--json")
        scenario.require(
            snapshot.returncode == 0,
            "the current `inspect` client cannot talk to the released binary's runner"
            f" (exit {snapshot.returncode}): {snapshot.diagnostics()}",
        )
        parsed = snapshot.json_tail()
        scenario.require(isinstance(parsed, dict), "the snapshot is not a JSON object")
        if parsed.get("run_id") != run_id:
            scenario.fail(f"the snapshot names run {parsed.get('run_id')!r}, not {run_id!r}")
        scenario.note(
            "current `inspect` reads the released runner's control plane"
            f" (snapshot_version {parsed.get('snapshot_version')})"
        )

        preview = ctx.new.invoke("prune", "--dry-run", "--json")
        scenario.require(
            preview.returncode == 0,
            f"the current `prune --dry-run` exited {preview.returncode}:"
            f" {preview.diagnostics()}",
        )
        report = preview.json_tail() or {}
        candidates = [item.get("run_id") for item in report.get("candidates") or []]
        if run_id in candidates:
            scenario.fail(
                "the current `prune --dry-run` would reap the released binary's LIVE"
                " run — a mid-upgrade prune must never touch a live entry"
            )
        else:
            scenario.note(
                "current `prune --dry-run` leaves the released binary's live run alone"
            )

        ack = ctx.new.invoke("cancel", "--run-id", run_id)
        scenario.require(
            ack.returncode == 0,
            "the current `cancel` client cannot cancel the released binary's runner"
            f" (exit {ack.returncode}): {ack.diagnostics()}",
        )
        parsed_ack = ack.json_tail()
        if isinstance(parsed_ack, dict) and parsed_ack.get("accepted") is not True:
            scenario.fail(f"the released runner did not accept the cancel: {parsed_ack!r}")

        waited = ctx.new.invoke("wait", "--run-id", run_id, "--timeout", "30s",
                                timeout=90.0)
        scenario.require(
            waited.returncode == 0,
            "the current `wait` did not observe the released binary's run finish"
            f" (exit {waited.returncode}): {waited.diagnostics()}",
        )
        scenario.note("current `cancel` + `wait` drive the released binary's run to its end")
    finally:
        release.write_text("release\n", encoding="utf-8")
        ctx.new.invoke("prune", "--json")


def scenario_old_clients_over_new_runner(ctx: Context, scenario: Scenario) -> None:
    """Released `list`/`inspect`/`cancel`/`wait` over the current build's live run."""
    ctx.require_surface(
        scenario,
        old=["list", "list:--json", "inspect", "inspect:--run-id", "inspect:--json",
             "cancel", "cancel:--run-id", "wait", "wait:--run-id", "wait:--timeout"],
        new=["run", "run:--detach", "run:--jsonl", "run:--run-id", "run:--timeout"],
    )
    ctx.reset_registry()
    workspace = ctx.scratch("new-runner")
    run_id = "interop-new-runner"
    release = workspace / "release-new-runner"
    start_detached(ctx, ctx.new, scenario, run_id, workspace / "new-runner.jsonl", release)

    try:
        entry = poll(lambda: entry_for(ctx.old, scenario, run_id))
        scenario.require(
            entry is not None,
            "the released `list --json` cannot see the run the current build"
            " registered — either a registry-record version bump or a shape the"
            " released reader rejects would hide a live run from an operator"
            " mid-upgrade",
        )
        scenario.require(
            entry.get("health") == "live",
            f"the released `list --json` reports health {entry.get('health')!r} for a run"
            " the current build has live",
        )
        scenario.note("released `list --json` reads the current build's live record")

        snapshot = ctx.old.invoke("inspect", "--run-id", run_id, "--json")
        scenario.require(
            snapshot.returncode == 0,
            "the released `inspect` client cannot talk to the current build's runner"
            f" (exit {snapshot.returncode}): {snapshot.diagnostics()}"
            " — a control-plane snapshot version bump lands here",
        )
        parsed = snapshot.json_tail()
        scenario.require(isinstance(parsed, dict), "the snapshot is not a JSON object")
        if parsed.get("run_id") != run_id:
            scenario.fail(f"the snapshot names run {parsed.get('run_id')!r}, not {run_id!r}")
        scenario.note(
            "released `inspect` reads the current runner's control plane"
            f" (snapshot_version {parsed.get('snapshot_version')})"
        )

        ack = ctx.old.invoke("cancel", "--run-id", run_id)
        scenario.require(
            ack.returncode == 0,
            "the released `cancel` client cannot cancel the current build's runner"
            f" (exit {ack.returncode}): {ack.diagnostics()}",
        )
        waited = ctx.old.invoke("wait", "--run-id", run_id, "--timeout", "30s",
                                timeout=90.0)
        scenario.require(
            waited.returncode == 0,
            "the released `wait` did not observe the current build's run finish"
            f" (exit {waited.returncode}): {waited.diagnostics()}",
        )
        scenario.note("released `cancel` + `wait` drive the current build's run to its end")
    finally:
        release.write_text("release\n", encoding="utf-8")
        ctx.new.invoke("prune", "--json")


def _stale_leftover(ctx: Context, scenario: Scenario, producer: Cli, reader: Cli,
                    run_id: str, tag: str) -> None:
    """Shared body of the two stale-leftover directions."""
    ctx.reset_registry()
    workspace = ctx.scratch(tag)
    release = workspace / f"release-{tag}"
    start_and_abandon(ctx, producer, scenario, run_id, workspace / f"{tag}.jsonl", release)
    try:
        entry = poll(
            lambda: next(
                (found for found in entries(reader, scenario)
                 if found.get("run_id") == run_id and found.get("health") == "stale"),
                None,
            )
        )
        if entry is None:
            observed = [(found.get("run_id"), found.get("health"))
                        for found in entries(reader, scenario)]
            scenario.require(
                False,
                f"the {reader.label} `list --json` never classified the"
                f" {producer.label} binary's abandoned record as stale; it sees"
                f" {observed}",
            )
        scenario.note(
            f"{reader.label} `list --json` classifies the {producer.label} binary's"
            " leftover as stale"
        )

        # `wait` over the leftover, before it is reaped: a confirmed-stale record
        # means the run is over, so the barrier must return at once rather than
        # block on a runner that is already gone.
        if ctx.supports(reader.label, "wait", "wait:--run-id", "wait:--timeout"):
            waited = reader.invoke("wait", "--run-id", run_id, "--timeout", "10s",
                                   timeout=45.0)
            if waited.returncode != 0:
                scenario.fail(
                    f"the {reader.label} `wait` did not treat the {producer.label}"
                    " binary's confirmed-stale leftover as a finished run (exit"
                    f" {waited.returncode}): {waited.diagnostics()}"
                )
            else:
                scenario.note(
                    f"{reader.label} `wait` returns at once on the {producer.label}"
                    " binary's leftover instead of blocking"
                )
        else:
            scenario.note(f"the {reader.label} binary predates `wait`; not exercised")

        preview = reader.invoke("prune", "--dry-run", "--json")
        scenario.require(
            preview.returncode == 0,
            f"the {reader.label} `prune --dry-run` exited {preview.returncode}:"
            f" {preview.diagnostics()}",
        )
        report = preview.json_tail() or {}
        candidates = [item.get("run_id") for item in report.get("candidates") or []]
        if run_id not in candidates:
            scenario.fail(
                f"the {reader.label} `prune --dry-run` does not name the"
                f" {producer.label} binary's stale record as a candidate: {candidates}"
            )

        pruned = reader.invoke("prune", "--json")
        scenario.require(
            pruned.returncode == 0,
            f"the {reader.label} `prune --json` exited {pruned.returncode}:"
            f" {pruned.diagnostics()}",
        )
        tally = pruned.json_tail() or {}
        if not tally.get("pruned"):
            scenario.fail(
                f"the {reader.label} `prune` reaped nothing, though the"
                f" {producer.label} binary left a confirmed-stale record: {tally!r}"
            )
        remaining = [found.get("run_id") for found in entries(reader, scenario)]
        if run_id in remaining:
            scenario.fail(
                f"the {producer.label} binary's stale record survived the"
                f" {reader.label} `prune`"
            )
        else:
            scenario.note(
                f"{reader.label} `prune` reaps the {producer.label} binary's leftover"
                f" ({tally.get('pruned')} record(s))"
            )
    finally:
        release.write_text("release\n", encoding="utf-8")
        ctx.new.invoke("prune", "--json")


def scenario_stale_from_old(ctx: Context, scenario: Scenario) -> None:
    """The current build reaps a record the released binary abandoned."""
    ctx.require_surface(
        scenario,
        old=["run", "run:--jsonl", "run:--run-id", "run:--timeout", "list", "list:--json"],
        new=["list", "list:--json", "prune", "prune:--json", "prune:--dry-run"],
    )
    _stale_leftover(ctx, scenario, ctx.old, ctx.new, "interop-stale-old", "stale-old")


def scenario_stale_from_new(ctx: Context, scenario: Scenario) -> None:
    """The released binary reaps a record the current build abandoned."""
    ctx.require_surface(
        scenario,
        old=["list", "list:--json", "prune", "prune:--json", "prune:--dry-run"],
        new=["run", "run:--jsonl", "run:--run-id", "run:--timeout", "list", "list:--json"],
    )
    _stale_leftover(ctx, scenario, ctx.new, ctx.old, "interop-stale-new", "stale-new")


def scenario_jsonl_schema(ctx: Context, scenario: Scenario) -> None:
    """Cross-read each binary's durable JSONL stream under the other's schema."""
    if jsonschema is None:
        scenario.skip("the `jsonschema` package is not installed")
    load_old_schema(ctx)
    ctx.reset_registry()

    workspace = ctx.scratch("streams")
    for which, cli in (("old", ctx.old), ("new", ctx.new)):
        events = workspace / f"{which}-teardown.jsonl"
        if produce_teardown_stream(ctx, cli, events, workspace / f"never-{which}"):
            ctx.streams[cli.label].append(events)

    scenario.require(
        bool(ctx.streams["old"]),
        "no JSONL stream could be recorded from the released binary, so nothing was"
        " cross-read under the current schema",
    )

    # A `schema_version` bump is the declared way to break this stream, so every
    # difference below becomes a licensed, reported break rather than a defect.
    report = scenario.warn if ctx.schema_version_bumped else scenario.fail
    if ctx.schema_version_bumped:
        scenario.warn(
            "the JSONL schema_version differs between the two binaries"
            f" ({ctx.old_probe.get('schema_version')} to"
            f" {ctx.new_probe.get('schema_version')}), so the differences below are"
            " declared breaks rather than drift"
        )

    # Upgrade direction: the released binary's durable stream, read with the
    # current build's published schema.
    relaxed_new = relax_for_upgrade_read(ctx.new_schema)
    additive_lines = 0
    for stream in ctx.streams["old"]:
        for index, line in stream_lines(stream):
            errors = validate(relaxed_new, line)
            if errors:
                report(
                    f"{stream.name}:{index} — the released binary emitted an event the"
                    " current schema rejects even with additive fields allowed:"
                    f" {errors[0]}"
                )
            elif validate(ctx.new_schema, line):
                additive_lines += 1
    scenario.note(
        "released streams validate against the current"
        f" fixtures/schema/v1/schema.json ({additive_lines} line(s) differ from it"
        " only by fields added since the release)"
    )

    # Downgrade direction: the current build's stream, read with the released
    # binary's own schema.
    if ctx.old_schema is None:
        scenario.note(
            "downgrade direction not exercised: no schema document is obtainable from"
            f" the released binary ({ctx.old_schema_origin or 'no source available'})"
        )
        return
    known = set(event_shapes(ctx.old_schema))
    relaxed_old = relax_for_downgrade_read(ctx.old_schema)
    unknown_tags: set[str] = set()
    for stream in ctx.streams["new"]:
        for index, line in stream_lines(stream):
            tag = line.get("event")
            if tag not in known:
                unknown_tags.add(str(tag))
                continue
            errors = validate(relaxed_old, line)
            if errors:
                report(
                    f"{stream.name}:{index} — the current build emits an event the"
                    " released schema cannot read even with additive fields tolerated:"
                    f" {errors[0]}"
                )
    scenario.note(
        "current streams validate against the released binary's own schema (from"
        f" {ctx.old_schema_origin}); event types added since the release:"
        f" {', '.join(sorted(unknown_tags)) or 'none'}"
    )

    drift = compare_event_shapes(ctx.old_schema, ctx.new_schema)
    for removal in drift.breaking:
        report(
            f"the JSONL schema dropped `{removal}` since the release — a removal is a"
            " breaking change no mid-upgrade reader can absorb"
        )
    scenario.note(
        f"schema drift since the release: +{len(drift.added_events)} event type(s),"
        f" +{len(drift.added_fields)} field(s), -{len(drift.removed_events)} event"
        f" type(s), -{len(drift.removed_fields)} field(s)"
    )
    if drift.added_fields:
        scenario.note(f"added fields: {', '.join(drift.added_fields)}")


def scenario_machine_output_schemas(ctx: Context, scenario: Scenario) -> None:
    """The released binary's stdout JSON under the current CLI schema documents."""
    if jsonschema is None:
        scenario.skip("the `jsonschema` package is not installed")
    cli_dir = ctx.schema_dir / "cli"
    if not cli_dir.is_dir():
        scenario.skip(f"{cli_dir} does not exist")
    ctx.require_surface(
        scenario,
        old=["probe", "probe:--json", "run", "run:--detach", "run:--jsonl",
             "run:--run-id", "run:--timeout", "list", "list:--json", "inspect",
             "inspect:--run-id", "inspect:--json", "cancel", "cancel:--run-id",
             "wait", "wait:--run-id", "wait:--timeout", "prune", "prune:--json"],
        new=[],
    )
    ctx.reset_registry()
    workspace = ctx.scratch("machine-output")
    run_id = "interop-machine-output"
    release = workspace / "release-machine-output"
    payloads: list[tuple[str, str, Any]] = []
    observed: dict[str, Any] = {}

    # Drive the released binary through its own lifecycle so each published output
    # family has a real payload — an empty registry would validate nothing.
    start_detached(ctx, ctx.old, scenario, run_id,
                   workspace / "machine-output.jsonl", release)
    try:
        probe = ctx.old.invoke("probe", "--json")
        scenario.require(probe.returncode == 0, "the released `probe --json` stopped working")
        report = probe.json_tail()
        payloads.append(("probe", "/$defs/probeReport", report))
        observed["probe_version"] = (report or {}).get("probe_version")

        listing = ctx.old.invoke("list", "--json")
        scenario.require(
            listing.returncode == 0,
            f"the released `list --json` exited {listing.returncode}:"
            f" {listing.diagnostics()}",
        )
        for line in listing.stdout.splitlines():
            if line.strip():
                payloads.append(("list", "/$defs/listEntry", json.loads(line)))

        snapshot = ctx.old.invoke("inspect", "--run-id", run_id, "--json")
        scenario.require(
            snapshot.returncode == 0,
            f"the released `inspect --json` exited {snapshot.returncode}:"
            f" {snapshot.diagnostics()}",
        )
        parsed = snapshot.json_tail()
        payloads.append(("inspect", "/$defs/snapshot", parsed))
        observed["snapshot_version"] = (parsed or {}).get("snapshot_version")

        ack = ctx.old.invoke("cancel", "--run-id", run_id)
        scenario.require(
            ack.returncode == 0,
            f"the released `cancel` exited {ack.returncode}: {ack.diagnostics()}",
        )
        payloads.append(("control-ack", "/$defs/ack", ack.json_tail()))

        wait_args = ["wait", "--run-id", run_id, "--timeout", "30s"]
        reports_outcome = not missing_surface(ctx.old_probe, ["wait:--report-outcome"])
        if reports_outcome:
            wait_args.append("--report-outcome")
        waited = ctx.old.invoke(*wait_args, timeout=90.0)
        scenario.require(
            waited.returncode == 0,
            f"the released `wait` exited {waited.returncode}: {waited.diagnostics()}",
        )
        if reports_outcome:
            payloads.append(("wait", "/$defs/waitOutcome", waited.json_tail()))

        pruned = ctx.old.invoke("prune", "--json")
        scenario.require(
            pruned.returncode == 0,
            f"the released `prune --json` exited {pruned.returncode}:"
            f" {pruned.diagnostics()}",
        )
        payloads.append(("prune", "/$defs/tally", pruned.json_tail()))
    finally:
        release.write_text("release\n", encoding="utf-8")
        ctx.new.invoke("prune", "--json")

    version_pins = {family: pointer for family, _, pointer in VERSIONED_CLI_OUTPUTS}
    # Each versioned family's pin, read once in whichever form its document
    # publishes (`const` or an enumerated range — see VERSIONED_CLI_OUTPUTS). A
    # family absent from this map publishes no pin at all any more; the loop at the
    # end of this scenario reports that, and until then nothing about the family is
    # treated as declared, so its shape differences stay failures.
    pins: dict[str, VersionPin] = {}
    for family, _, pointer in VERSIONED_CLI_OUTPUTS:
        pin = version_pin(resolve_pointer(
            json.loads((cli_dir / f"{family}.schema.json").read_text(encoding="utf-8")),
            pointer,
        ))
        if pin is not None:
            pins[family] = pin
    # A family whose own version field was bumped has declared its break, so a
    # shape difference in it is licensed rather than a defect (see Scenario.warn).
    # "Bumped" is "the current document no longer admits what the release
    # publishes", not inequality against a single value: the inspect family's pin
    # is a *range*, and a released runner's snapshot_version 1 rendered by a build
    # that writes 2 is inside it and is no break at all. Reading that as a bump
    # would silently downgrade every genuine defect in the family's shape to a
    # declared break — the false negative this lane exists to catch.
    bumped = {
        family
        for family, field_name, _ in VERSIONED_CLI_OUTPUTS
        if observed.get(field_name) is not None
        and family in pins
        and not pins[family].admits(observed[field_name])
    }
    families: set[str] = set()
    for family, pointer, payload in payloads:
        if payload is None:
            scenario.fail(f"the released `{family}` output was not JSON")
            continue
        document = json.loads((cli_dir / f"{family}.schema.json").read_text(encoding="utf-8"))
        scenario.require(
            resolve_pointer(document, pointer) is not None,
            f"fixtures/schema/cli/{family}.schema.json has no `{pointer}` branch;"
            " this driver's pointer must be updated alongside the document",
        )
        if family in pins:
            document = without_version_pin(document, version_pins[family])
        errors = validate(relax_for_upgrade_read(rooted_at(document, pointer)), payload)
        if errors:
            (scenario.warn if family in bumped else scenario.fail)(
                f"the released `{family}` machine output is not readable under the"
                f" current fixtures/schema/cli/{family}.schema.json: {errors[0]}"
            )
        else:
            families.add(family)
    scenario.note(
        f"{len(payloads)} released payload(s) across {len(families)} published family/ies"
        f" ({', '.join(sorted(families)) or 'none'}) validate against the current"
        " fixtures/schema/cli/ documents"
    )

    # The two versioned families (see VERSIONED_CLI_OUTPUTS): a value drift here is
    # a deliberate breaking bump, and mixed-version operation stops working the
    # moment it lands, so it is reported loudly rather than absorbed.
    for family, field_name, pointer in VERSIONED_CLI_OUTPUTS:
        pin = pins.get(family)
        value = observed.get(field_name)
        if pin is None:
            # Reserved for the field genuinely going away: the node is absent, or
            # it constrains `field_name` to something that is not a version at all.
            # A pin that merely changed *form* is followed by `version_pin`, not
            # reported here — the checked scope is unchanged when that happens.
            scenario.fail(
                f"fixtures/schema/cli/{family}.schema.json no longer pins"
                f" `{field_name}` at `{pointer}` — that node is absent or carries"
                " neither a `const` nor an `enum` of versions; the versioned-output"
                " scope in docs/compatibility.md changed and this driver must follow"
            )
        elif value is None:
            scenario.note(f"{field_name}: not observable from the released binary")
        elif not pin.admits(value):
            scenario.warn(
                f"`{field_name}` was bumped: the released binary reports {value},"
                f" which the current fixtures/schema/cli/{family}.schema.json no"
                f" longer publishes ({pin.render()}). That field is versioned"
                " precisely because it is read by a party that did not invoke the"
                " binary, so the bump is the correct way to break it — but the break"
                " is real: this must ship as a breaking release, and a consumer"
                " pinning the old value stops working the moment it does"
            )
        elif len(pin.accepted) == 1:
            scenario.note(f"{field_name} is unchanged at {pin.render()}")
        else:
            scenario.note(
                f"{field_name} {value} (released) is still inside the range the"
                f" current build publishes ({pin.render()})"
            )


# --------------------------------------------------------------------------- #
# Plumbing
# --------------------------------------------------------------------------- #


def validate(schema: dict, payload: Any) -> list[str]:
    """Every validation message `schema` produces for `payload` (empty when valid)."""
    validator = jsonschema.validators.validator_for(schema)(schema)
    return [error.message for error in validator.iter_errors(payload)]


def stream_lines(path: Path) -> list[tuple[int, dict]]:
    """A JSONL file as `(1-based line number, object)` pairs, blank lines dropped."""
    parsed: list[tuple[int, dict]] = []
    for index, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        stripped = raw.strip()
        if stripped:
            parsed.append((index, json.loads(stripped)))
    return parsed


CHILD_SOURCE = '''\
"""Idle until a release file appears, or until a deadline passes.

The payload of every run this interop lane launches: portable (it is the very
interpreter running the driver), silent, and always self-terminating, so a
cancelled CI job cannot leave it behind.
"""
import os
import sys
import time

release = sys.argv[1]
deadline = time.monotonic() + float(sys.argv[2])
while time.monotonic() < deadline and not os.path.exists(release):
    time.sleep(0.05)
'''


SCENARIOS: tuple[tuple[str, str, Callable[[Context, Scenario], None]], ...] = (
    ("probe reports",
     "both binaries self-report a usable compatibility surface",
     scenario_probe_reports),
    ("probe pinning (upgrade)",
     "the current build still satisfies the released binary's own pinning",
     scenario_probe_pinning_upgrade),
    ("probe pinning (downgrade)",
     "the released binary fails closed on what it cannot provide",
     scenario_probe_pinning_downgrade),
    ("new clients over an old runner",
     "current list/inspect/prune/cancel/wait over a released binary's live run",
     scenario_new_clients_over_old_runner),
    ("old clients over a new runner",
     "released list/inspect/cancel/wait over the current build's live run",
     scenario_old_clients_over_new_runner),
    ("stale leftover from the release",
     "the current build's prune reaps a record the released binary abandoned",
     scenario_stale_from_old),
    ("stale leftover from the current build",
     "the released binary's prune reaps a record the current build abandoned",
     scenario_stale_from_new),
    ("JSONL schema cross-reading",
     "each binary's durable stream read under the other's schema",
     scenario_jsonl_schema),
    ("machine-output schemas",
     "the released binary's stdout JSON under the current fixtures/schema/cli/",
     scenario_machine_output_schemas),
)


def run_scenarios(ctx: Context) -> list[Scenario]:
    results: list[Scenario] = []
    for name, summary, body in SCENARIOS:
        scenario = Scenario(name=name, summary=summary)
        try:
            body(ctx, scenario)
        except Skipped as skipped:
            scenario.status = "skip"
            scenario.skip_reason = str(skipped)
        except Failed:
            pass  # already recorded by `Scenario.require`
        except Exception as unexpected:  # noqa: BLE001 - a crash here is a lane failure
            scenario.fail(f"{type(unexpected).__name__}: {unexpected}")
        results.append(scenario)
        echo(scenario)
    return results


def echo(scenario: Scenario) -> None:
    marker = {"pass": "PASS", "warn": "WARN", "fail": "FAIL", "skip": "SKIP"}
    print(f"[{marker[scenario.status]}] {scenario.name} — {scenario.summary}", flush=True)
    if scenario.skip_reason:
        print(f"       skipped: {scenario.skip_reason}", flush=True)
    for note in scenario.notes:
        print(f"       - {note}", flush=True)
    for warning in scenario.warnings:
        print(f"       ~ declared break: {warning}", flush=True)
    for failure in scenario.failures:
        print(f"       ! {failure}", flush=True)


def render_summary(ctx: Context, results: list[Scenario]) -> str:
    tally = {status: sum(1 for item in results if item.status == status)
             for status in ("pass", "warn", "fail", "skip")}
    lines = [
        "### Cross-version interop",
        "",
        f"Released binary `{ctx.old_probe.get('version', 'unknown')}`"
        f" against the current build `{ctx.new_probe.get('version', 'unknown')}`:"
        f" {tally['pass']} passed, {tally['warn']} with a declared break,"
        f" {tally['fail']} failed, {tally['skip']} skipped.",
        "",
        "| Scenario | Result | Detail |",
        "| --- | --- | --- |",
    ]
    for item in results:
        if item.status == "skip":
            detail = item.skip_reason
        else:
            detail = "; ".join(item.failures or item.warnings or item.notes)
        lines.append(f"| {item.name} | {item.status} | {detail.replace('|', '-') or '-'} |")
    lines.append("")
    if tally["fail"]:
        lines.append(
            "A **failure** means the mixed-version contract documented in"
            " `docs/compatibility.md` no longer holds between the last published"
            " release and the current build, with no version field declaring the"
            " change."
        )
    if tally["warn"]:
        lines.append(
            "A **declared break** is a bump of one of the versioned contracts"
            " (`schema_version`, `snapshot_version`, `probe_version`). It is the"
            " sanctioned way to break compatibility and is not a lane failure, but it"
            " does mean mixed-version operation stops working, so the change has to"
            " ship as a breaking release."
        )
    if tally["skip"] == len(results):
        lines.append(
            "Every scenario was skipped, so this run proved nothing — see the reasons"
            " above."
        )
    return "\n".join(lines) + "\n"


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    parser.add_argument("--old-binary", required=True, type=Path,
                        help="the latest published release binary")
    parser.add_argument("--new-binary", required=True, type=Path,
                        help="the binary built from the current checkout")
    parser.add_argument("--schema-dir", default=Path("fixtures/schema"), type=Path,
                        help="the checkout's fixtures/schema directory")
    parser.add_argument("--work-dir", type=Path,
                        help="scratch directory (a temporary one is used when omitted)")
    parser.add_argument("--archive-schema", type=Path,
                        help="schema/schema.json unpacked from the release archive, used"
                             " when the released binary predates `probe --print-schema`")
    parser.add_argument("--summary-file", type=Path,
                        help="append a Markdown summary here (e.g. $GITHUB_STEP_SUMMARY)")
    parser.add_argument("--json-report", type=Path,
                        help="write the machine-readable scenario report here")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    for label, path in (("old", args.old_binary), ("new", args.new_binary)):
        if not path.is_file():
            print(f"error: the {label} binary {path} does not exist", file=sys.stderr)
            return 2
    schema_path = args.schema_dir / "v1" / "schema.json"
    if not schema_path.is_file():
        print(f"error: {schema_path} does not exist", file=sys.stderr)
        return 2
    if jsonschema is None:
        print("warning: the `jsonschema` package is missing, so the two schema"
              " scenarios will report as skipped", file=sys.stderr)

    temporary: str | None = None
    if args.work_dir is None:
        temporary = tempfile.mkdtemp(prefix="processkit-interop-")
        work_dir = Path(temporary)
    else:
        work_dir = args.work_dir
        work_dir.mkdir(parents=True, exist_ok=True)

    try:
        registry_dir = work_dir / "registry"
        registry_dir.mkdir(parents=True, exist_ok=True)
        child_script = work_dir / "interop_child.py"
        child_script.write_text(CHILD_SOURCE, encoding="utf-8")
        ctx = Context(
            old=Cli("old", args.old_binary.resolve(), registry_dir),
            new=Cli("new", args.new_binary.resolve(), registry_dir),
            work_dir=work_dir,
            schema_dir=args.schema_dir,
            child_script=child_script,
            new_schema=json.loads(schema_path.read_text(encoding="utf-8")),
            archive_schema=args.archive_schema,
        )
        results = run_scenarios(ctx)

        summary = render_summary(ctx, results)
        print()
        print(summary, end="")
        if args.summary_file is not None:
            with args.summary_file.open("a", encoding="utf-8") as handle:
                handle.write(summary)
        if args.json_report is not None:
            args.json_report.write_text(
                json.dumps(
                    {
                        "old_version": ctx.old_probe.get("version"),
                        "new_version": ctx.new_probe.get("version"),
                        "scenarios": [
                            {
                                "name": item.name,
                                "status": item.status,
                                "skip_reason": item.skip_reason,
                                "notes": item.notes,
                                "declared_breaks": item.warnings,
                                "failures": item.failures,
                            }
                            for item in results
                        ],
                    },
                    indent=2,
                ) + "\n",
                encoding="utf-8",
            )
    finally:
        if temporary is not None:
            shutil.rmtree(temporary, ignore_errors=True)
    return 1 if any(item.status == "fail" for item in results) else 0


if __name__ == "__main__":
    sys.exit(main())
