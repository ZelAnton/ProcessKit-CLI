//! Through-the-binary tests for the per-user run registry and its first control-plane
//! client (`AGENTS.md`, "Testing tiers"): a normal run creates a registry entry while
//! it is live and removes it on a clean exit, a runner-imposed ending (a `--timeout`)
//! tears the entry down too, and `inspect` reaches a live run over the registry +
//! local transport (or fails with the reserved `CONTROL` code when the run cannot be
//! reached). These prove the *binary's* registry/control lifecycle end-to-end; the
//! fine-grained mechanics — owner-only permissions, stale detection, concurrency, the
//! wire snapshot — are unit-tested in `src/registry.rs` and `src/control.rs`.
//!
//! Each test points the runner *and* the inspect client at an isolated scratch
//! registry via the `PROCESSKIT_CLI_REGISTRY_DIR` override so they never touch the
//! real per-user registry and parallel tests never collide.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{bin, command_with_flags, scratch, shell_inline};

/// The registry directory the runner is pointed at, kept separate from the
/// `--jsonl` events file (which lands in `scratch_dir` itself) so scanning for
/// records never trips over the event stream.
fn registry_dir(scratch_dir: &Path) -> PathBuf {
    scratch_dir.join("registry")
}

/// How many record files (`*.json`) the registry directory holds right now. A
/// missing directory or unreadable entry counts as zero.
fn record_count(dir: &Path) -> usize {
    match fs::read_dir(dir) {
        Ok(read_dir) => read_dir
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
            .count(),
        Err(_) => 0,
    }
}

/// Read the sole record file's text, asserting there is exactly one.
fn read_only_record(dir: &Path) -> String {
    let mut records: Vec<PathBuf> = fs::read_dir(dir)
        .expect("registry directory exists once a run has started")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    assert_eq!(records.len(), 1, "expected exactly one registry record");
    fs::read_to_string(records.pop().unwrap()).expect("read the registry record")
}

/// Poll `cond` until it holds or `timeout` elapses (then panic).
fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < timeout,
            "condition was not met within {timeout:?}"
        );
        sleep(Duration::from_millis(50));
    }
}

/// A child that stays alive for ~2s — long enough to observe the live entry.
fn slow_child() -> Vec<String> {
    if cfg!(windows) {
        shell_inline("ping -n 3 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 2")
    }
}

/// A child that stays alive well past any test deadline (a `--timeout` ends it).
fn long_child() -> Vec<String> {
    if cfg!(windows) {
        shell_inline("ping -n 300 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 300")
    }
}

/// The core lifecycle: a run publishes a well-formed record while it is live, then a
/// clean exit removes it.
#[test]
fn run_creates_then_removes_the_registry_entry() {
    let dir = scratch("registry-clean");
    let registry = registry_dir(&dir);
    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &[],
        slow_child(),
    )
    .spawn()
    .expect("spawn the runner");

    // While the run is live, exactly one record exists and it is well-formed.
    wait_until(|| record_count(&registry) == 1, Duration::from_secs(10));
    let record = read_only_record(&registry);
    assert!(
        record.contains("\"run_id\""),
        "the record names the run: {record}"
    );
    assert!(
        record.contains("\"started_at\""),
        "the record carries a start time: {record}"
    );
    assert!(
        record.contains("\"endpoint\":\""),
        "a live run now publishes its control-transport endpoint (no longer null): {record}"
    );
    assert!(
        !record.contains("\"endpoint\":null"),
        "the endpoint is populated once the transport is up: {record}"
    );
    assert!(
        record.contains("advisory_lock"),
        "the record documents its liveness signal: {record}"
    );
    assert!(
        !record.contains("\"pid\""),
        "the record must not be keyed by PID: {record}"
    );

    // A clean exit removes the entry.
    let status = child.wait().expect("the runner exits");
    assert!(status.success(), "the child exits cleanly");
    assert_eq!(
        record_count(&registry),
        0,
        "a clean exit must remove the registry entry"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Removal is not limited to the happy path: a `--timeout` teardown removes the
/// entry too.
#[test]
fn timeout_teardown_removes_the_registry_entry() {
    let dir = scratch("registry-timeout");
    let registry = registry_dir(&dir);
    let child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--timeout", "3s"],
        long_child(),
    )
    .spawn()
    .expect("spawn the runner");

    // The entry appears while the run is live...
    wait_until(|| record_count(&registry) == 1, Duration::from_secs(3));

    // ...the deadline ends the run with the reserved TIMEOUT code...
    let out = child.wait_with_output().expect("the runner exits");
    assert_eq!(
        out.status.code(),
        Some(106),
        "a timeout exits with the reserved TIMEOUT code"
    );

    // ...and that runner-imposed teardown removes the entry, just like a clean exit.
    assert_eq!(
        record_count(&registry),
        0,
        "a timeout teardown must remove the registry entry"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// A child that stays alive ~5s — long enough to inspect the live run, short enough
/// that the run exits cleanly (removing its entry) without a kill.
fn inspectable_child() -> Vec<String> {
    if cfg!(windows) {
        shell_inline("ping -n 6 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 5")
    }
}

/// Run `inspect --run-id <id> --json` against the same scratch registry as the run
/// under test, and wait for it to finish.
fn inspect(registry: &Path, run_id: &str) -> Output {
    Command::new(bin())
        .args(["inspect", "--run-id", run_id, "--json"])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .expect("spawn the inspect client")
}

/// The happy path: `inspect` finds a live runner through the registry, connects over
/// the local transport, and prints a machine-readable snapshot of the run — its id,
/// containment mechanism, root PID, members, and start time.
#[test]
fn inspect_reports_a_live_run() {
    let dir = scratch("inspect-live");
    let registry = registry_dir(&dir);
    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "inspect-me"],
        inspectable_child(),
    )
    .spawn()
    .expect("spawn the runner");

    // The run is inspectable once its record (and thus its endpoint) is published.
    wait_until(|| record_count(&registry) == 1, Duration::from_secs(10));

    let out = inspect(&registry, "inspect-me");
    assert_eq!(
        out.status.code(),
        Some(0),
        "inspecting a live run succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snapshot: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("inspect prints a JSON snapshot line");
    assert_eq!(
        snapshot["run_id"], "inspect-me",
        "the snapshot names the run: {snapshot}"
    );
    assert!(
        snapshot["mechanism"].is_string(),
        "the snapshot names the containment mechanism: {snapshot}"
    );
    assert!(
        snapshot["started_at"].is_string(),
        "the snapshot carries a start time: {snapshot}"
    );
    assert!(
        snapshot["members"].is_array(),
        "the snapshot carries a members array: {snapshot}"
    );
    assert!(
        snapshot.get("root_pid").is_some(),
        "the snapshot carries a root_pid field (possibly null): {snapshot}"
    );
    assert_eq!(
        snapshot["snapshot_version"], 1,
        "the snapshot carries its format version: {snapshot}"
    );

    // Let the run finish cleanly (removing its own entry).
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// A run id that is not registered is a distinguishable failure: the reserved
/// `CONTROL` code (103), a message naming the run on stderr, and no snapshot — never a
/// hang or a generic error.
#[test]
fn inspect_reports_no_such_run_with_the_control_code() {
    let dir = scratch("inspect-missing");
    let registry = registry_dir(&dir);

    let out = inspect(&registry, "ghost");
    assert_eq!(
        out.status.code(),
        Some(103),
        "an unknown run id is a CONTROL failure; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "a failed inspect prints no snapshot: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ghost"),
        "the failure names the run: {stderr}"
    );

    let _ = fs::remove_dir_all(&dir);
}
