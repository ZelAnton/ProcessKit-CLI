//! Through-the-binary tests for the per-user run registry and its control-plane
//! clients (`AGENTS.md`, "Testing tiers"): a normal run creates a registry entry
//! while it is live and removes it on a clean exit, a runner-imposed ending (a
//! `--timeout`) tears the entry down too, `inspect` reaches a live run over the
//! registry + local transport, and the mutating `cancel`/`kill` verbs reach the same
//! live runner and end it with their own reserved exit codes — each falling back to
//! the reserved `CONTROL` code when the run cannot be reached. These prove the
//! *binary's* registry/control lifecycle end-to-end; the fine-grained mechanics —
//! owner-only permissions, stale detection, concurrency, the wire snapshot, verb
//! routing — are unit-tested in `src/registry.rs` and `src/control.rs`.
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

/// `inspect` gets the identical hard "ambiguous run id" failure as `cancel`/`kill`
/// when two concurrent live runs share a `--run-id` — a deliberate, documented
/// choice (`docs/registry.md`, "Run id resolution"): a snapshot of the wrong run
/// would be exactly as misleading as acting on it, so there is no softer fallback.
#[test]
fn inspect_reports_ambiguous_run_id_for_duplicate_run_ids() {
    let dir = scratch("inspect-ambiguous");
    let registry = registry_dir(&dir);
    let run_id = "dup-inspect";

    let mut first = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", run_id],
        long_child(),
    )
    .spawn()
    .expect("spawn the first runner");
    let mut second = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", run_id],
        long_child(),
    )
    .spawn()
    .expect("spawn the second runner");

    wait_until(|| record_count(&registry) == 2, Duration::from_secs(10));

    let out = inspect(&registry, run_id);
    assert_eq!(
        out.status.code(),
        Some(103),
        "an ambiguous run id is a CONTROL failure for inspect; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "an ambiguous inspect prints no snapshot: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ambiguous"),
        "the inspect failure names the reason: {stderr}"
    );

    let _ = first.kill();
    let _ = first.wait();
    let _ = second.kill();
    let _ = second.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// Run a mutating control verb (`cancel`/`kill --run-id <id>`) against the same
/// scratch registry as the run under test, and wait for it to finish.
fn control_client(registry: &Path, verb: &str, run_id: &str) -> Output {
    Command::new(bin())
        .args([verb, "--run-id", run_id])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .expect("spawn the control client")
}

/// A `cancel` command reaches the live runner over the control plane and ends the
/// run through the shared soft-stop → grace → hard-kill teardown: the client is
/// acked (exit 0) and the *run* exits with the reserved `CONTROL_CANCELLED` code
/// (108) — distinct from a Ctrl-C (107) and a timeout (106). The teardown removes
/// the registry entry, like every other decided ending.
#[test]
fn cancel_ends_a_live_run_with_the_control_cancel_code() {
    let dir = scratch("control-cancel");
    let registry = registry_dir(&dir);
    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "cancel-me"],
        long_child(),
    )
    .spawn()
    .expect("spawn the runner");

    // The run is reachable once its record (and thus its endpoint) is published.
    wait_until(|| record_count(&registry) == 1, Duration::from_secs(10));

    let out = control_client(&registry, "cancel", "cancel-me");
    assert_eq!(
        out.status.code(),
        Some(0),
        "cancelling a live run succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let status = child.wait().expect("the runner exits after the cancel");
    assert_eq!(
        status.code(),
        Some(108),
        "a control-plane cancel ends the run with CONTROL_CANCELLED (108)"
    );
    assert_eq!(
        record_count(&registry),
        0,
        "a control cancel teardown must remove the registry entry"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// A `kill` command reaches the live runner and hard-kills the whole tree
/// immediately: the client is acked (exit 0) and the run exits with the reserved
/// `CONTROL_KILLED` code (109), distinct from every other ending.
#[test]
fn kill_ends_a_live_run_with_the_control_kill_code() {
    let dir = scratch("control-kill");
    let registry = registry_dir(&dir);
    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "kill-me"],
        long_child(),
    )
    .spawn()
    .expect("spawn the runner");

    wait_until(|| record_count(&registry) == 1, Duration::from_secs(10));

    let out = control_client(&registry, "kill", "kill-me");
    assert_eq!(
        out.status.code(),
        Some(0),
        "killing a live run succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let status = child.wait().expect("the runner exits after the kill");
    assert_eq!(
        status.code(),
        Some(109),
        "a control-plane kill ends the run with CONTROL_KILLED (109)"
    );
    assert_eq!(
        record_count(&registry),
        0,
        "a control kill teardown must remove the registry entry"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Two concurrent runs started with the *same* explicit `--run-id` both register and
/// stay live at once (the registry never enforces `run_id` uniqueness — see
/// `docs/registry.md`, "Run id resolution"). Against that ambiguity, `cancel`/`kill`
/// must refuse to guess which run to act on: a deterministic, documented `CONTROL`
/// (103) "ambiguous run id" failure, never picking whichever entry the directory
/// scan happens to return first.
#[test]
fn cancel_and_kill_report_ambiguous_run_id_for_duplicate_run_ids() {
    let dir = scratch("control-ambiguous");
    let registry = registry_dir(&dir);
    let run_id = "dup-me";

    let mut first = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", run_id],
        long_child(),
    )
    .spawn()
    .expect("spawn the first runner");
    let mut second = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", run_id],
        long_child(),
    )
    .spawn()
    .expect("spawn the second runner");

    // Both runs are live and reachable at once — the ambiguity `cancel`/`kill` must
    // detect.
    wait_until(|| record_count(&registry) == 2, Duration::from_secs(10));

    for verb in ["cancel", "kill"] {
        let out = control_client(&registry, verb, run_id);
        assert_eq!(
            out.status.code(),
            Some(103),
            "an ambiguous run id is a CONTROL failure for {verb}; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stdout.is_empty(),
            "an ambiguous {verb} prints no ack: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("ambiguous"),
            "the {verb} failure names the reason: {stderr}"
        );
        assert!(
            stderr.contains(run_id),
            "the {verb} failure names the run: {stderr}"
        );

        // Neither run was touched by the rejected command — both stay live.
        assert_eq!(
            record_count(&registry),
            2,
            "a rejected ambiguous {verb} must not end either run"
        );
    }

    // Clean up both still-live runners directly (never through the ambiguous id).
    let _ = first.kill();
    let _ = first.wait();
    let _ = second.kill();
    let _ = second.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// An unknown run id is the same distinguishable failure for the mutating verbs as
/// for `inspect`: the reserved `CONTROL` code (103), a message naming the action and
/// the run on stderr, and no ack on stdout — never a hang.
#[test]
fn cancel_and_kill_report_no_such_run_with_the_control_code() {
    let dir = scratch("control-missing");
    let registry = registry_dir(&dir);

    for verb in ["cancel", "kill"] {
        let out = control_client(&registry, verb, "ghost");
        assert_eq!(
            out.status.code(),
            Some(103),
            "an unknown run id is a CONTROL failure for {verb}; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stdout.is_empty(),
            "a failed {verb} prints no ack: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("ghost"),
            "the {verb} failure names the run: {stderr}"
        );
        assert!(
            stderr.contains(verb),
            "the failure names the action `{verb}`: {stderr}"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

/// Run `list [--json]` against `registry` and wait for it to finish.
fn list(registry: &Path, json: bool) -> Output {
    let mut cmd = Command::new(bin());
    cmd.arg("list");
    if json {
        cmd.arg("--json");
    }
    cmd.env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .expect("spawn the list client")
}

/// An empty registry is not an error: `list` exits `0` either way, printing an
/// empty JSON-lines result for `--json` and a plain notice for the human-readable
/// form.
#[test]
fn list_reports_an_empty_registry() {
    let dir = scratch("list-empty");
    let registry = registry_dir(&dir);

    let out = list(&registry, false);
    assert_eq!(
        out.status.code(),
        Some(0),
        "an empty registry is not an error; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no runs registered"),
        "the human-readable form notes the registry is empty: {stdout}"
    );

    let out = list(&registry, true);
    assert_eq!(
        out.status.code(),
        Some(0),
        "an empty registry is not an error for --json either; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "--json prints no lines for an empty registry: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let _ = fs::remove_dir_all(&dir);
}

/// `list --json` finds a live run through the same registry scan `inspect` uses and
/// prints its `run_id`, health, `started_at`, and `endpoint` as one JSON line — the
/// discovery counterpart to `inspect`/`cancel`/`kill` for a caller that does not
/// already know the `run_id`.
#[test]
fn list_reports_a_live_run() {
    let dir = scratch("list-live");
    let registry = registry_dir(&dir);
    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "list-me"],
        inspectable_child(),
    )
    .spawn()
    .expect("spawn the runner");

    // The run is listable once its record (and thus its endpoint) is published.
    wait_until(|| record_count(&registry) == 1, Duration::from_secs(10));

    let out = list(&registry, true);
    assert_eq!(
        out.status.code(),
        Some(0),
        "listing a registry with a live run succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "exactly one entry is registered: {stdout}");
    let entry: serde_json::Value =
        serde_json::from_str(lines[0]).expect("list --json prints one valid JSON line per entry");
    assert_eq!(
        entry["run_id"], "list-me",
        "the entry names the run: {entry}"
    );
    assert_eq!(
        entry["health"], "live",
        "the live entry reports health live: {entry}"
    );
    assert!(
        entry["started_at"].is_string(),
        "the entry carries a start time: {entry}"
    );
    assert!(
        entry["endpoint"].is_string(),
        "a live run has published its control-transport endpoint: {entry}"
    );

    // The human-readable form also names the run and its health.
    let out = list(&registry, false);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("list-me") && stdout.contains("live"),
        "the human-readable form names the run and its health: {stdout}"
    );

    // Let the run finish cleanly (removing its own entry).
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// `list` is documented as read-only and must never mutate registry state just to
/// scan it: listing an empty (never-yet-created) registry must leave the registry
/// directory absent, not create it as a side effect of the scan.
#[test]
fn list_does_not_create_the_registry_directory() {
    let dir = scratch("list-no-create");
    let registry = registry_dir(&dir);
    assert!(
        !registry.exists(),
        "the scratch registry directory starts absent"
    );

    let out = list(&registry, false);
    assert_eq!(
        out.status.code(),
        Some(0),
        "listing a never-created registry is not an error; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !registry.exists(),
        "a read-only `list` must not create the registry directory as a side effect"
    );

    let _ = fs::remove_dir_all(&dir);
}
