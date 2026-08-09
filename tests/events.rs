//! Through-the-binary tests for the JSONL lifecycle-event stream: events land in
//! the `--jsonl` file and never on stdout, every line carries `schema_version`,
//! and the terminal `runner_exit` preserves the child's own code — including on a
//! runner-own failure where the child never ran. These prove the *wiring* of the
//! schema through the shipped binary (`AGENTS.md`, "Testing tiers"); the exact
//! wire shapes are pinned separately by the in-crate golden test
//! (`src/events.rs`), since a live stream's timestamps/PIDs/run-id are not
//! deterministic.
//!
//! This module also validates the golden fixture — and, where a test already has
//! a live stream in hand, the events actually emitted by the binary — against the
//! published, machine-readable JSON Schema (`fixtures/schema/v1/schema.json`).
//! That keeps the schema honest against the same material the in-crate golden
//! test pins byte-for-byte and the through-the-binary tests exercise live
//! (`docs/schema.md`, "This is the normative description").

mod common;

use std::path::Path;
use std::sync::OnceLock;

use common::{command_with_flags, events_path, run, run_with_flags, scratch, shell_inline};
use jsonschema::Validator;
use processkit_cli::events_cmd::schema::SchemaChecker;
use serde_json::Value;

/// The compiled schema validator for `fixtures/schema/v1/schema.json`, built once
/// and shared by every test in this module.
fn schema_validator() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/schema/v1/schema.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read schema {}: {err}", path.display()));
        let schema: Value = serde_json::from_str(&text).expect("schema.json is valid JSON");
        jsonschema::validator_for(&schema)
            .expect("schema.json is a valid JSON Schema (draft 2020-12)")
    })
}

/// Assert every event in `events` validates against the published JSON Schema,
/// collecting every violation before panicking so a shape drift reports every
/// offending line in one failure rather than only the first.
fn assert_events_match_schema(events: &[Value]) {
    let validator = schema_validator();
    let failures: Vec<String> = events
        .iter()
        .enumerate()
        .filter_map(|(i, event)| {
            let errs: Vec<String> = validator
                .iter_errors(event)
                .map(|e| e.to_string())
                .collect();
            (!errs.is_empty())
                .then(|| format!("line {}: {event}\n    {}", i + 1, errs.join("\n    ")))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "event(s) did not validate against fixtures/schema/v1/schema.json:\n{}",
        failures.join("\n")
    );
}

/// Every line of the golden fixture, parsed.
fn golden_fixture_events() -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/schema/v1/events.jsonl");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read golden fixture {}: {err}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each fixture line is valid JSON"))
        .collect()
}

/// Systematic single-point mutations of `event`: for every property (and every
/// property of every nested object), drop it, and replace it with each of a
/// string, a number, and `null`; plus one extra unknown property, and one unknown
/// `event` tag. Every one of these *should* be rejected by the schema — but this
/// module deliberately does not assume that: what the differential test below
/// asserts is that the shipped checker and the reference engine agree on each,
/// whatever the verdict is.
fn mutations(event: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(object) = event.as_object() else {
        return out;
    };

    let replacements = [
        Value::String("mutated".to_string()),
        Value::Number(12345.into()),
        Value::Null,
    ];
    for key in object.keys() {
        let mut dropped = object.clone();
        dropped.remove(key);
        out.push(Value::Object(dropped));

        for replacement in &replacements {
            let mut changed = object.clone();
            changed.insert(key.clone(), replacement.clone());
            out.push(Value::Object(changed));
        }

        // One level of nesting, which is as deep as this schema goes
        // (`command`, `stdout`/`stderr`, `shutdown`, and the `members` items).
        if let Some(nested) = object[key].as_object() {
            for nested_key in nested.keys() {
                for replacement in &replacements {
                    let mut inner = nested.clone();
                    inner.insert(nested_key.clone(), replacement.clone());
                    let mut changed = object.clone();
                    changed.insert(key.clone(), Value::Object(inner));
                    out.push(Value::Object(changed));
                }
                let mut inner = nested.clone();
                inner.remove(nested_key);
                let mut changed = object.clone();
                changed.insert(key.clone(), Value::Object(inner));
                out.push(Value::Object(changed));
            }
        }
    }

    let mut extra = object.clone();
    extra.insert("invented_field".to_string(), Value::Bool(true));
    out.push(Value::Object(extra));

    let mut retagged = object.clone();
    retagged.insert("event".to_string(), Value::String("teleported".to_string()));
    out.push(Value::Object(retagged));

    out
}

/// **The shipped `events --validate` checker agrees with a real JSON Schema
/// engine.**
///
/// `src/events_cmd/schema.rs` interprets the embedded schema document itself
/// rather than linking a JSON Schema crate into the binary (see that module's
/// "Why not a JSON Schema engine"). What makes that safe is not the argument in
/// its doc comment — it is this test: for the golden fixture and for a generated
/// corpus of single-point mutations of it, the two implementations must return the
/// *same verdict* on every document. A subset validator that quietly ignored a
/// keyword would answer "valid" where this one answers "invalid", and fail here.
#[test]
fn the_shipped_checker_agrees_with_the_reference_engine() {
    let reference = schema_validator();
    let shipped = SchemaChecker::compile().expect("the embedded schema compiles");

    let mut checked = 0usize;
    let mut disagreements = Vec::new();
    let mut rejected = 0usize;
    for event in golden_fixture_events() {
        for candidate in std::iter::once(event.clone()).chain(mutations(&event)) {
            checked += 1;
            let theirs = reference.is_valid(&candidate);
            let ours = shipped.conforms(&candidate);
            if !theirs {
                rejected += 1;
            }
            if theirs != ours {
                disagreements.push(format!("reference={theirs} shipped={ours} for {candidate}"));
            }
        }
    }

    assert!(
        disagreements.is_empty(),
        "{} of {checked} documents got different verdicts:
{}",
        disagreements.len(),
        disagreements
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join(
                "
"
            )
    );
    // Guard against a vacuous pass: the corpus must actually contain documents the
    // schema rejects, or "the two agree" would only mean "both said yes to
    // everything".
    assert!(
        checked > 500 && rejected > 100,
        "the corpus must exercise both verdicts: {checked} checked, {rejected} rejected"
    );
}

/// The golden fixture (`fixtures/schema/v1/events.jsonl`) — one representative of
/// every v1 event type, in both `run_started` redaction branches — validates
/// line-for-line against the published schema.
#[test]
fn golden_fixture_validates_against_the_schema() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/schema/v1/events.jsonl");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read golden fixture {}: {err}", path.display()));
    let events: Vec<Value> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each fixture line is valid JSON"))
        .collect();
    assert!(!events.is_empty(), "the golden fixture must not be empty");
    assert_events_match_schema(&events);
}

/// Read the emitted event stream for `dir` and parse each non-empty line as JSON,
/// panicking if any line is not a well-formed object — a malformed stream is a
/// contract violation.
fn read_events(dir: &Path) -> Vec<Value> {
    let path = events_path(dir);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read events file {}: {err}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("each event line is valid JSON"))
        .collect()
}

/// The `event` type tag of each parsed event, in order.
fn event_types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| {
            e["event"]
                .as_str()
                .expect("event tag is a string")
                .to_string()
        })
        .collect()
}

/// Whether `v` is a JSON string of 64 lowercase-hex characters — the shape of an
/// `argv_sha256` fingerprint.
fn is_sha256_hex(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// The `command` object of the (first) `run_started` event in `dir`'s stream.
fn run_started_command(dir: &Path) -> Value {
    read_events(dir)
        .iter()
        .find(|e| e["event"] == "run_started")
        .map(|e| e["command"].clone())
        .expect("a run_started event")
}

/// A completed run writes its lifecycle events to the `--jsonl` file, every line
/// carries `schema_version`, and none of it leaks onto the child's stdout.
#[test]
fn events_go_to_the_jsonl_file_and_never_stdout() {
    let dir = scratch("events-file");
    let out = run(&dir, &[], shell_inline("echo hello-child"));
    assert_eq!(out.status.code(), Some(0), "the child exits cleanly");

    let events = read_events(&dir);
    assert!(!events.is_empty(), "the run must have written events");
    for event in &events {
        assert_eq!(
            event["schema_version"], 1,
            "every event carries schema_version=1: {event}"
        );
        assert!(
            event.get("event").and_then(Value::as_str).is_some(),
            "every event carries a string type tag: {event}"
        );
    }
    assert_events_match_schema(&events);

    let types = event_types(&events);
    for expected in ["run_started", "root_exited", "runner_exit"] {
        assert!(
            types.iter().any(|t| t == expected),
            "the stream must contain `{expected}`: saw {types:?}"
        );
    }
    assert_eq!(
        types.last().map(String::as_str),
        Some("runner_exit"),
        "runner_exit must be the terminal event: {types:?}"
    );

    // The child's own output reaches our stdout; no JSON event leaks there.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hello-child"),
        "child stdout passes through"
    );
    assert!(
        !stdout.contains("schema_version") && !stdout.contains("run_started"),
        "events must never appear on the child's stdout: {stdout:?}"
    );
}

/// `run_started` reports the child's root PID, containment mechanism, the
/// platform's abrupt-owner-death guarantee, and a redacted command by default.
#[test]
fn run_started_reports_root_pid_mechanism_and_redacts_the_command() {
    let dir = scratch("run-started");
    let out = run(&dir, &[], shell_inline("exit 0"));
    assert_eq!(out.status.code(), Some(0));

    let events = read_events(&dir);
    let started = events
        .iter()
        .find(|e| e["event"] == "run_started")
        .expect("a run_started event");

    assert!(
        started["root_pid"].as_u64().is_some(),
        "root_pid should be present for a launched child: {started}"
    );
    let mechanism = started["mechanism"].as_str().expect("mechanism string");
    assert!(
        [
            "job_object",
            "cgroup_v2",
            "process_group",
            "process_reaper",
            "unknown",
        ]
        .contains(&mechanism),
        "mechanism must be one of the documented values, got {mechanism:?}"
    );
    let expected_abrupt_cleanup = if cfg!(windows) {
        "whole_tree"
    } else if cfg!(target_os = "linux") {
        "direct_child_only"
    } else {
        "none"
    };
    assert_eq!(
        started["abrupt_cleanup"], expected_abrupt_cleanup,
        "run_started must report the guarantee that survives abrupt runner death: {started}"
    );

    let command = &started["command"];
    assert_eq!(command["redacted"], true, "argv is redacted by default");
    assert!(command["argv"].is_null(), "no raw argv without --argv-raw");
    assert!(
        is_sha256_hex(&command["argv_sha256"]),
        "a redacted command still carries a hex argv fingerprint: {command}"
    );
    assert!(
        command["hint"].is_null(),
        "a plain shell command is not a recognized worker shape: {command}"
    );
}

/// The binary resolves a relative `--cwd` exactly as child spawn does, but records
/// an absolute path to the same directory so a JSONL consumer never needs the
/// runner's ambient working directory to interpret `run_started.cwd`. The spelling
/// need not preserve a platform path alias (`/var` and `/private/var` on macOS).
#[test]
fn run_started_normalizes_a_relative_cwd_to_absolute() {
    let dir = scratch("run-started-relative-cwd");
    let child_cwd = dir.join("child");
    std::fs::create_dir_all(&child_cwd).expect("create the child working directory");

    let mut command = command_with_flags(&dir, &[], &["--cwd", "child"], shell_inline("exit 0"));
    command.current_dir(&dir);
    let out = command.output().expect("spawn the runner binary");
    assert_eq!(
        out.status.code(),
        Some(0),
        "the child runs successfully in the relative cwd; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let events = read_events(&dir);
    let started = events
        .iter()
        .find(|event| event["event"] == "run_started")
        .expect("a run_started event");
    let reported = started["cwd"]
        .as_str()
        .expect("run_started.cwd is a string when it resolves");
    let reported = Path::new(reported);
    assert!(reported.is_absolute(), "the event cwd is absolute");
    assert_eq!(
        std::fs::canonicalize(reported).expect("resolve the reported cwd"),
        std::fs::canonicalize(&child_cwd).expect("resolve the requested child cwd"),
        "the event identifies the same filesystem directory the child used"
    );
}

/// `--argv-raw` records the raw argv verbatim while the fingerprint is still
/// computed (it never depends on disclosure); a plain command carries no hint.
#[test]
fn argv_raw_records_the_raw_command() {
    let dir = scratch("argv-raw");
    let out = run_with_flags(&dir, &[], &["--argv-raw"], shell_inline("exit 0"));
    assert_eq!(out.status.code(), Some(0));

    let command = run_started_command(&dir);
    assert_eq!(command["redacted"], false, "--argv-raw disables redaction");
    let argv = command["argv"].as_array().expect("raw argv array");
    assert!(
        !argv.is_empty() && argv.iter().all(|a| a.is_string()),
        "raw argv is recorded as strings: {command}"
    );
    assert!(
        is_sha256_hex(&command["argv_sha256"]),
        "the fingerprint is computed even under --argv-raw: {command}"
    );
    assert!(
        command["hint"].is_null(),
        "a plain shell command is not a recognized worker shape even raw: {command}"
    );
}

/// The `run_started` command `hint` classifies a recognized worker shape (an
/// MSBuild reusable-worker argv) and leaves an ordinary command unclassified, while
/// `argv_sha256` is filled in both cases. The marker tokens ride along as inert
/// arguments to a shell no-op (`rem` on Windows, `:` elsewhere), so the child exits
/// cleanly on both platforms while the runner still records them in argv.
#[test]
fn run_started_hint_classifies_msbuild_and_leaves_unknown_shapes_null() {
    let msbuild = if cfg!(windows) {
        shell_inline("rem MSBuild.dll /nodemode:1 /nodeReuse:true")
    } else {
        shell_inline(": MSBuild.dll /nodemode:1 /nodeReuse:true")
    };
    let dir = scratch("hint-msbuild");
    let out = run(&dir, &[], msbuild);
    assert_eq!(out.status.code(), Some(0), "the no-op child exits cleanly");

    let command = run_started_command(&dir);
    assert_eq!(
        command["hint"], "msbuild_node_reuse",
        "an MSBuild reusable-worker argv is classified: {command}"
    );
    assert!(
        is_sha256_hex(&command["argv_sha256"]),
        "the fingerprint is filled alongside the hint: {command}"
    );

    // An ordinary command shares the fingerprint contract but has no hint.
    let plain = scratch("hint-plain");
    let out = run(&plain, &[], shell_inline("exit 0"));
    assert_eq!(out.status.code(), Some(0));
    let command = run_started_command(&plain);
    assert!(
        command["hint"].is_null(),
        "an unrecognized shape leaves the hint null: {command}"
    );
    assert!(
        is_sha256_hex(&command["argv_sha256"]),
        "...while the fingerprint is still filled: {command}"
    );
}

/// The child's exact code is forwarded *and* recorded in the terminal
/// `runner_exit`, whose `child_code` preserves it separately from `code`.
#[test]
fn runner_exit_records_the_child_code() {
    let dir = scratch("child-code");
    let out = run(&dir, &[], shell_inline("exit 7"));
    assert_eq!(
        out.status.code(),
        Some(7),
        "the child's code passes through"
    );

    let events = read_events(&dir);
    let root_exited = events
        .iter()
        .find(|e| e["event"] == "root_exited")
        .expect("a root_exited event");
    assert_eq!(root_exited["outcome"], "exited");
    assert_eq!(root_exited["code"], 7);

    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(runner_exit["event"], "runner_exit");
    assert_eq!(runner_exit["code"], 7);
    assert_eq!(runner_exit["source"], "child_exit");
    assert_eq!(
        runner_exit["child_code"], 7,
        "the child's own code is preserved in runner_exit"
    );
}

/// With `--capture-dir`, an `output_captured` event lands after the teardown pair
/// and before the terminal `runner_exit`, carrying per-stream capture metadata; and
/// **without** the flag no such event appears — a plain run's stream is unchanged
/// (backward compatibility).
#[test]
fn capture_dir_adds_output_captured_and_its_absence_is_unchanged() {
    // A run that captures.
    let dir = scratch("events-capture");
    let capture_dir = dir.join("cap");
    let capture_flag = capture_dir.to_string_lossy().into_owned();
    let out = run_with_flags(
        &dir,
        &[],
        &["--capture-dir", &capture_flag],
        shell_inline("echo captured-line"),
    );
    assert_eq!(out.status.code(), Some(0));

    let events = read_events(&dir);
    assert_events_match_schema(&events);
    let types = event_types(&events);
    assert!(
        types.iter().any(|t| t == "output_captured"),
        "capture must emit output_captured: {types:?}"
    );
    // Positioned after cleanup_finished and before the terminal runner_exit.
    let captured_at = types.iter().position(|t| t == "output_captured").unwrap();
    let cleanup_at = types
        .iter()
        .position(|t| t == "cleanup_finished")
        .expect("cleanup_finished present");
    assert!(
        cleanup_at < captured_at && captured_at < types.len() - 1,
        "output_captured sits after cleanup and before runner_exit: {types:?}"
    );
    assert_eq!(types.last().map(String::as_str), Some("runner_exit"));

    let captured = events
        .iter()
        .find(|e| e["event"] == "output_captured")
        .unwrap();
    assert!(
        captured["stdout"]["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("stdout.log")),
        "the stdout capture path is reported: {captured}"
    );
    assert!(
        is_sha256_hex(&captured["stdout"]["sha256"]),
        "the stdout capture carries a content hash: {captured}"
    );
    assert_eq!(captured["stdout"]["truncated"], false);
    let _ = std::fs::remove_dir_all(&dir);

    // The same run without --capture-dir emits no output_captured at all.
    let plain = scratch("events-no-capture");
    let out = run(&plain, &[], shell_inline("echo captured-line"));
    assert_eq!(out.status.code(), Some(0));
    let plain_types = event_types(&read_events(&plain));
    assert!(
        !plain_types.iter().any(|t| t == "output_captured"),
        "a run without --capture-dir must not emit output_captured: {plain_types:?}"
    );
    let _ = std::fs::remove_dir_all(&plain);
}

/// A spawn failure records `spawn_failed` and a `runner_exit` whose `child_code`
/// is null (the child never ran), and writes nothing to the child's stdout.
#[test]
fn spawn_failure_records_spawn_failed_and_a_null_child_code() {
    let dir = scratch("spawn-fail");
    let out = run(&dir, &[], ["processkit_cli_no_such_program_xyz"]);
    assert_eq!(
        out.status.code(),
        Some(101),
        "spawn failure uses the SPAWN code"
    );
    assert!(out.stdout.is_empty(), "nothing on the child's stdout");

    let events = read_events(&dir);
    assert_events_match_schema(&events);
    let types = event_types(&events);
    assert!(
        !types.iter().any(|t| t == "run_started"),
        "no run_started when the child never started: {types:?}"
    );
    let spawn_failed = events
        .iter()
        .find(|e| e["event"] == "spawn_failed")
        .expect("a spawn_failed event");
    assert_eq!(spawn_failed["code"], 101);

    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(runner_exit["event"], "runner_exit");
    assert_eq!(runner_exit["source"], "spawn_error");
    assert_eq!(runner_exit["code"], 101);
    assert!(
        runner_exit["child_code"].is_null(),
        "no child code is fabricated for a child that never ran: {runner_exit}"
    );
}

/// An uncreatable `--capture-dir` is a fail-closed **setup** failure (T-158): the
/// child never runs, the runner exits with the reserved `SETUP` code (111) rather
/// than `INTERNAL` (104, kept for genuine runner-logic faults), and its terminal
/// `runner_exit` agrees — `source: "setup"`, the same code, and a null `child_code`.
/// The emitted stream validates against the published schema, which lists `setup`
/// among the `runner_exit` sources.
#[test]
fn an_uncreatable_capture_dir_is_a_setup_failure_with_a_null_child_code() {
    let dir = scratch("setup-capture-fail");
    // A regular file cannot be a parent directory, so creating a capture dir beneath
    // it fails the same way on every platform (ENOTDIR / "directory name is invalid").
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, b"not a directory\n").expect("write the blocker file");
    let capture_flag = blocker.join("cap").to_string_lossy().into_owned();

    let out = run_with_flags(
        &dir,
        &[],
        &["--capture-dir", &capture_flag],
        shell_inline("echo should-not-run"),
    );
    assert_eq!(
        out.status.code(),
        Some(111),
        "an uncreatable --capture-dir uses the reserved SETUP code, not INTERNAL: stderr {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "the child never ran, so nothing reaches its stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let events = read_events(&dir);
    assert_events_match_schema(&events);
    let types = event_types(&events);
    assert!(
        !types.iter().any(|t| t == "run_started"),
        "no run_started when the setup failed before the child spawned: {types:?}"
    );

    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(runner_exit["event"], "runner_exit");
    assert_eq!(
        runner_exit["source"], "setup",
        "the terminal event names the setup source: {runner_exit}"
    );
    assert_eq!(
        runner_exit["code"], 111,
        "the terminal event carries the reserved SETUP code: {runner_exit}"
    );
    assert!(
        runner_exit["child_code"].is_null(),
        "no child code is fabricated for a child that never ran: {runner_exit}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A capture setup that fails on its **second** stream is the same fail-closed
/// SETUP failure as an uncreatable `--capture-dir` above — and, unlike before
/// T-326, it leaves nothing of its own behind: the `stdout.log` the attempt had
/// already created is rolled back, so no empty transcript survives to be mistaken
/// for a real one, and no `output_captured` event claims a transcript that was
/// never taken. Once the cause is cleared, a retry into the same directory runs
/// the child and captures both streams normally.
#[test]
fn a_failed_second_capture_stream_rolls_back_and_a_retry_captures() {
    let dir = scratch("capture-second-stream-fails");
    let capture_dir = dir.join("cap");
    std::fs::create_dir(&capture_dir).expect("create the capture directory");
    // A directory cannot be opened for writing on any platform (EISDIR /
    // ERROR_ACCESS_DENIED), so this fails the *second* stream's setup after the
    // first one has already been opened — the ordering this test is about.
    std::fs::create_dir(capture_dir.join("stderr.log")).expect("block stderr.log with a directory");
    let capture_flag = capture_dir.to_string_lossy().into_owned();

    let out = run_with_flags(
        &dir,
        &[],
        &["--capture-dir", &capture_flag],
        shell_inline("echo should-not-run"),
    );
    assert_eq!(
        out.status.code(),
        Some(111),
        "an unopenable capture stream is a fail-closed SETUP failure: stderr {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "the child never ran, so nothing reaches its stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !capture_dir.join("stdout.log").exists(),
        "the failed attempt rolls back the stdout.log it created"
    );
    assert!(
        capture_dir.join("stderr.log").is_dir(),
        "the pre-existing directory that blocked the setup is not what gets removed"
    );

    let events = read_events(&dir);
    assert_events_match_schema(&events);
    let types = event_types(&events);
    assert!(
        !types.iter().any(|t| t == "output_captured"),
        "no transcript was taken, so no output_captured is emitted: {types:?}"
    );
    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(runner_exit["event"], "runner_exit");
    assert_eq!(
        runner_exit["source"], "setup",
        "the terminal event names the setup source: {runner_exit}"
    );
    assert_eq!(runner_exit["code"], 111);
    assert!(
        runner_exit["child_code"].is_null(),
        "no child code is fabricated for a child that never ran: {runner_exit}"
    );

    // Clear the cause and retry into the same capture directory. The retry writes
    // its own `--jsonl` file (a fresh scratch dir), so the two runs' event streams
    // stay separate; only this run's own invariants are asserted, never a
    // comparison of per-run facts across the two invocations.
    std::fs::remove_dir(capture_dir.join("stderr.log")).expect("clear the blocking directory");
    let retry = scratch("capture-second-stream-retry");
    let out = run_with_flags(
        &retry,
        &[],
        &["--capture-dir", &capture_flag],
        shell_inline("echo captured-after-retry"),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "the retry runs the child normally: stderr {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let retry_events = read_events(&retry);
    assert_events_match_schema(&retry_events);
    let retry_types = event_types(&retry_events);
    assert!(
        retry_types.iter().any(|t| t == "output_captured"),
        "the retry captures, so it emits output_captured: {retry_types:?}"
    );
    assert_eq!(retry_types.last().map(String::as_str), Some("runner_exit"));
    let stdout_log = std::fs::read_to_string(capture_dir.join("stdout.log"))
        .expect("the retry creates the stdout transcript");
    assert!(
        stdout_log.contains("captured-after-retry"),
        "the retry's transcript holds the child's output: {stdout_log:?}"
    );
    assert!(
        capture_dir.join("stderr.log").is_file(),
        "the retry creates the second stream's file where the blocking directory was"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&retry);
}

/// `members_snapshot` reports the enriched per-member fields (`ppid`, `name`,
/// `start_time`) ProcessKit's `members_info()` fills, on every platform this
/// crate's CI runs (Windows, Linux, macOS all report them per the platform matrix
/// in `docs/schema.md`, "Enriched member fields" — only the "bare" BSDs, outside
/// the CI matrix, report `null`). The child sleeps briefly so it is still present
/// when the snapshot is taken right after `run_started`.
#[test]
fn members_snapshot_reports_enriched_fields() {
    let dir = scratch("members-snapshot-enriched");
    let brief_sleep = if cfg!(windows) {
        shell_inline("ping -n 2 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 1")
    };
    let out = run(&dir, &[], brief_sleep);
    assert_eq!(out.status.code(), Some(0));

    let events = read_events(&dir);
    assert_events_match_schema(&events);

    let started = events
        .iter()
        .find(|e| e["event"] == "run_started")
        .expect("a run_started event");
    let root_pid = started["root_pid"].as_u64().expect("root_pid is present");

    let snapshot = events
        .iter()
        .find(|e| e["event"] == "members_snapshot")
        .expect("a members_snapshot event");
    assert_eq!(
        snapshot["reason"], "spawn",
        "the snapshot every run emits after spawn names itself as such, live through \
         the binary and not only in the golden fixture: {snapshot}"
    );
    assert_eq!(
        snapshot["read_error"], false,
        "`read_error` is always present, and `false` on a successful read, on the \
         default (no `--snapshot-interval`) path too — so an empty `members` array \
         is never ambiguous between an observation and a failed read: {snapshot}"
    );
    let members = snapshot["members"].as_array().expect("members is an array");
    let root = members
        .iter()
        .find(|m| m["pid"].as_u64() == Some(root_pid))
        .unwrap_or_else(|| panic!("the snapshot must list the root child: {snapshot}"));

    assert!(
        root["ppid"].as_u64().is_some(),
        "ppid is populated on this platform: {root}"
    );
    assert!(
        root["name"].as_str().is_some(),
        "the executable name is populated on this platform: {root}"
    );
    assert!(
        root["start_time"].as_str().is_some(),
        "the start-time token is populated on this platform: {root}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A `--timeout` that elapses emits a `timeout` event and the cleanup pair, then a
/// terminal `runner_exit` in the reserved band with a null child code. Kept
/// cross-platform via a runtime `cfg!` so both OSes compile and lint this test.
#[test]
fn timeout_emits_timeout_cleanup_and_runner_exit() {
    let dir = scratch("timeout-events");
    let long_sleep = if cfg!(windows) {
        shell_inline("ping -n 300 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 300")
    };
    let out = run_with_flags(
        &dir,
        &[],
        &["--timeout", "1s", "--grace", "500ms"],
        long_sleep,
    );
    assert_eq!(
        out.status.code(),
        Some(106),
        "a timeout uses the reserved code"
    );

    let events = read_events(&dir);
    assert_events_match_schema(&events);
    let types = event_types(&events);
    for expected in [
        "run_started",
        "timeout",
        "cleanup_started",
        "cleanup_finished",
        "runner_exit",
    ] {
        assert!(
            types.iter().any(|t| t == expected),
            "the timeout stream must contain `{expected}`: {types:?}"
        );
    }

    let timeout = events
        .iter()
        .find(|e| e["event"] == "timeout")
        .expect("a timeout event");
    assert_eq!(timeout["timeout_ms"], 1000);
    assert_eq!(
        timeout["reason"], "overall",
        "a whole-run --timeout is reported with reason=overall: {timeout}"
    );

    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(runner_exit["event"], "runner_exit");
    assert_eq!(runner_exit["source"], "timeout");
    assert_eq!(runner_exit["code"], 106);
    assert!(
        runner_exit["child_code"].is_null(),
        "a runner-imposed ending forwards no child code: {runner_exit}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A `--idle-timeout` that elapses on a **silent** child reuses the whole `--timeout`
/// machinery — the same reserved `TIMEOUT` (106) code, the same `timeout` runner-exit
/// `source`, and the same cleanup pair — distinguished only by the `timeout` event's
/// `reason: "idle"`. A long-lived child that produces no output is the silence the
/// idle deadline is for. Cross-platform via a runtime `cfg!`.
#[test]
fn idle_timeout_emits_timeout_with_idle_reason() {
    let dir = scratch("idle-timeout-events");
    // A long sleeper that writes nothing to stdout: the runner observes no output, so
    // the idle window elapses and ends the run.
    let long_silent = if cfg!(windows) {
        shell_inline("ping -n 300 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 300")
    };
    let out = run_with_flags(
        &dir,
        &[],
        &["--idle-timeout", "1s", "--grace", "500ms"],
        long_silent,
    );
    assert_eq!(
        out.status.code(),
        Some(106),
        "an idle timeout reuses the reserved TIMEOUT code, not a new one"
    );

    let events = read_events(&dir);
    assert_events_match_schema(&events);
    let types = event_types(&events);
    for expected in [
        "run_started",
        "timeout",
        "cleanup_started",
        "cleanup_finished",
        "runner_exit",
    ] {
        assert!(
            types.iter().any(|t| t == expected),
            "the idle-timeout stream must contain `{expected}`: {types:?}"
        );
    }

    let timeout = events
        .iter()
        .find(|e| e["event"] == "timeout")
        .expect("a timeout event");
    assert_eq!(
        timeout["reason"], "idle",
        "the --idle-timeout trigger is reported with reason=idle: {timeout}"
    );
    assert_eq!(
        timeout["timeout_ms"], 1000,
        "timeout_ms echoes the idle window that elapsed"
    );

    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(runner_exit["event"], "runner_exit");
    assert_eq!(
        runner_exit["source"], "timeout",
        "an idle expiry reuses the `timeout` source, not a new one"
    );
    assert_eq!(runner_exit["code"], 106);
    assert!(
        runner_exit["child_code"].is_null(),
        "a runner-imposed ending forwards no child code: {runner_exit}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
