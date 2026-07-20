//! Through-the-binary tests for the per-user run registry (`AGENTS.md`, "Testing
//! tiers"): a normal run creates a registry entry while it is live and removes it on
//! a clean exit, and a runner-imposed ending (a `--timeout`) tears the entry down
//! too. These prove the *binary's* registry lifecycle end-to-end; the fine-grained
//! mechanics — owner-only permissions, stale detection, concurrency — are
//! unit-tested in `src/registry.rs`.
//!
//! Each test points the runner at an isolated scratch registry via the
//! `PROCESSKIT_CLI_REGISTRY_DIR` override so it never touches the real per-user
//! registry and parallel tests never collide.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{command_with_flags, scratch, shell_inline};

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
        record.contains("\"endpoint\":null"),
        "the connection endpoint is reserved (null) until T-008: {record}"
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
