//! Through-the-binary tests for the JSONL lifecycle-event stream: events land in
//! the `--jsonl` file and never on stdout, every line carries `schema_version`,
//! and the terminal `runner_exit` preserves the child's own code — including on a
//! runner-own failure where the child never ran. These prove the *wiring* of the
//! schema through the shipped binary (`AGENTS.md`, "Testing tiers"); the exact
//! wire shapes are pinned separately by the in-crate golden test
//! (`src/events.rs`), since a live stream's timestamps/PIDs/run-id are not
//! deterministic.

mod common;

use std::path::Path;

use common::{events_path, run, run_with_flags, scratch, shell_inline};
use serde_json::Value;

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

/// `run_started` reports the child's root PID, the containment mechanism, and a
/// redacted command by default (no raw argv, no fabricated hash).
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
        ["job_object", "cgroup_v2", "process_group"].contains(&mechanism),
        "mechanism must be one of the documented values, got {mechanism:?}"
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
