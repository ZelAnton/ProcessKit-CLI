//! Through-the-binary tests for the per-user run registry and its control-plane
//! clients (`AGENTS.md`, "Testing tiers"): a normal run creates a registry entry
//! while it is live and removes it on a clean exit, a runner-imposed ending (a
//! `--timeout`) tears the entry down too, `inspect` reaches a live run over the
//! registry + local transport, and the mutating `cancel`/`kill` verbs reach the same
//! live runner and end it with their own reserved exit codes — each falling back to
//! the reserved `CONTROL` code when the run cannot be reached. `wait` is the
//! registry-only client alongside them: it blocks on a live run's entry until the run
//! is gone, gives up with its own reserved `WAIT_TIMEOUT` code, and refuses an
//! ambiguous id, all without ever contacting a runner. These prove the
//! *binary's* registry/control lifecycle end-to-end; the fine-grained mechanics —
//! owner-only permissions, stale detection, concurrency, the wire snapshot, verb
//! routing — are unit-tested in `src/registry/mod.rs` and `src/control/tests.rs`.
//!
//! Each test points the runner *and* the inspect client at an isolated scratch
//! registry via the `PROCESSKIT_CLI_REGISTRY_DIR` override so they never touch the
//! real per-user registry and parallel tests never collide.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{bin, command_with_flags, scratch, shell_inline};
use processkit_cli::registry::test_support::{
    write_stale_entry, write_stale_entry_with_labels, write_unprobeable_entry,
};

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

/// How long a fixture waits for the registry to reach an expected state (a record
/// appearing, a second run registering, an endpoint being published) before giving up.
///
/// Deliberately generous: every condition here is reached by *another process* —
/// spawning the runner binary, standing up its control transport and publishing its
/// record — and on a heavily loaded host that whole chain can cost seconds rather than
/// the few hundred milliseconds it takes on an idle one. The budget costs nothing when
/// the condition holds (the poll returns immediately), so the only thing a tight value
/// buys is intermittent failures.
const RECORD_WAIT: Duration = Duration::from_secs(20);

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

/// Set `path`'s mtime `age` in the past, without a real sleep — used to age an
/// orphaned-lock fixture past `Registry`'s `ORPHAN_LOCK_MIN_AGE` (`src/registry/mod.rs`,
/// [R-01]) so `prune`'s orphan-lock pass actually treats it as a candidate, rather
/// than the fixture racing that floor purely by test timing. Keep the age passed by
/// callers comfortably above that constant's value.
#[cfg(unix)]
fn backdate(path: &Path, age: Duration) {
    use std::fs::File;
    use std::time::SystemTime;

    let file = File::open(path).expect("open the fixture to backdate its mtime");
    file.set_modified(SystemTime::now() - age)
        .expect("backdate the fixture's mtime");
}

#[cfg(windows)]
fn backdate(path: &Path, age: Duration) {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::time::SystemTime;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .expect("open the fixture to backdate its mtime");
    file.set_modified(SystemTime::now() - age)
        .expect("backdate the fixture's mtime");
}

/// A child that stays alive for ~6s — long enough to observe the live entry by
/// scanning the registry directory directly, with room for the runner itself to take
/// seconds to start and register on a loaded host (the same margin reasoning as
/// [`inspectable_child`], which additionally has to survive whole client
/// invocations).
fn slow_child() -> Vec<String> {
    if cfg!(windows) {
        shell_inline("ping -n 7 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 6")
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
    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);
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
        &["--timeout", "8s"],
        long_child(),
    )
    .spawn()
    .expect("spawn the runner");

    // The entry appears while the run is live — the one observation window here that
    // [`RECORD_WAIT`] cannot cover, since the run's own deadline above ends it (and
    // deletes the record) at 8s. The two are sized together: enough room for the
    // runner to start and register on a loaded host, with slack left before the
    // teardown this test is actually about.
    wait_until(|| record_count(&registry) == 1, Duration::from_secs(6));

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

/// A child that stays alive ~15s — long enough to observe the live run through one or
/// more *client invocations* (`inspect`/`list`/`wait`, each its own process spawn),
/// short enough that the run still exits cleanly (removing its entry) without a kill.
///
/// The window is the fixture's whole point, so it is sized for a loaded host rather
/// than an idle one: every observation here costs at least one binary spawn plus a
/// registry scan, and a second run registering (or a waiter taking its snapshot) can
/// add several more seconds under full `cargo test` parallelism. A ~5s window was
/// enough on an idle machine but flaked reproducibly under host contention — the run
/// simply ended mid-test, surfacing as "no run with that id is registered" or as a
/// `wait_until` that could never reach its count. Keep this comfortably above
/// [`RECORD_WAIT`]-scale delays; it costs wall-clock time in the fixtures that let the
/// run finish on its own, and nothing else.
fn inspectable_child() -> Vec<String> {
    if cfg!(windows) {
        shell_inline("ping -n 16 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 15")
    }
}

/// A child that stays alive ~30s — for the scenarios whose live window has to span
/// *two* whole client invocations rather than one.
///
/// [`inspectable_child`] is sized for one observation: spawn a client, let it resolve
/// the run and report. Three tests here need the run to still be live at a *second*
/// client spawn — `inspect_all_snapshots_live_runs_with_conjunctive_labels` (JSON then
/// human aggregate `inspect`, each taking a full process-tree snapshot of the match),
/// `list_reports_a_live_run` (JSON then human `list`), and
/// `list_filters_by_labels_and_health_with_shared_semantics` (two filtered `list`s
/// around a stale-fixture write) — and every cost inside that window is paid twice.
///
/// Measured, not guessed: at the ~15s window these were exactly the fixtures that still
/// failed when the registry suite was run two, three, and four ways in parallel on an
/// already loaded host, always on the *second* invocation and always with the run
/// already gone ("no live runs to inspect", "no runs registered"). Doubling the window
/// costs wall-clock time only in these three tests — they wait for their runs to exit
/// anyway, and they run alongside the rest of the suite — so the fixtures that merely
/// let one run finish keep the shorter window.
fn twice_inspectable_child() -> Vec<String> {
    if cfg!(windows) {
        shell_inline("ping -n 31 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 30")
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
    let capture = dir.join("capture");
    let capture_arg = capture.to_string_lossy().into_owned();
    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "inspect-me", "--capture-dir", &capture_arg],
        inspectable_child(),
    )
    .spawn()
    .expect("spawn the runner");

    // The run is inspectable once its record (and thus its endpoint) is published.
    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);

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
    let members = snapshot["members"]
        .as_array()
        .expect("the snapshot carries a members array");
    let root_pid = snapshot["root_pid"]
        .as_u64()
        .expect("the snapshot carries a root_pid");
    let root = members
        .iter()
        .find(|m| m["pid"].as_u64() == Some(root_pid))
        .unwrap_or_else(|| panic!("the snapshot must list the root child: {snapshot}"));
    // `inspect`'s members are enriched the same way as the JSONL `members_snapshot`
    // (`docs/schema.md`, "Enriched member fields") — populated on every platform
    // this crate's CI runs (only the "bare" BSDs, outside the CI matrix, report
    // `null`).
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
    assert_eq!(
        snapshot["snapshot_version"], 2,
        "the snapshot carries its format version: {snapshot}"
    );
    let jsonl = snapshot["jsonl"]
        .as_str()
        .unwrap_or_else(|| panic!("inspect publishes the JSONL locator: {snapshot}"));
    assert!(
        std::path::Path::new(jsonl).is_absolute(),
        "the JSONL locator is absolute: {jsonl}"
    );
    assert_eq!(snapshot["capture_dir"], capture_arg);

    // Let the run finish cleanly (removing its own entry).
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// T-214 regression: without `--json`, `inspect` prints the human-readable rendering,
/// not JSON — the `if json` branch in `inspect_async` (`src/control/mod.rs`) actually
/// takes the other arm on a real binary invocation, not just in a unit test of the
/// pure rendering function.
#[test]
fn inspect_reports_a_live_run_in_human_form() {
    let dir = scratch("inspect-live-human");
    let registry = registry_dir(&dir);
    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "inspect-me-human"],
        inspectable_child(),
    )
    .spawn()
    .expect("spawn the runner");

    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);

    let out = Command::new(bin())
        .args(["inspect", "--run-id", "inspect-me-human"])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", &registry)
        .output()
        .expect("spawn the inspect client without --json");
    assert_eq!(
        out.status.code(),
        Some(0),
        "inspecting a live run without --json succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        serde_json::from_str::<serde_json::Value>(&stdout).is_err(),
        "without --json, stdout must not itself parse as a single JSON value: {stdout}"
    );
    assert!(
        stdout.contains("run_id:") && stdout.contains("inspect-me-human"),
        "the human-readable form names the run: {stdout}"
    );
    assert!(
        stdout.contains("mechanism:"),
        "the human-readable form names the containment mechanism: {stdout}"
    );
    assert!(
        stdout.contains("jsonl:") && stdout.contains("capture_dir:"),
        "the human-readable form exposes artifact locators: {stdout}"
    );

    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn inspect_all_snapshots_live_runs_with_conjunctive_labels() {
    let dir = scratch("inspect-all");
    let registry = registry_dir(&dir);
    let ci_dir = dir.join("ci");
    let local_dir = dir.join("local");
    fs::create_dir_all(&ci_dir).expect("create CI run directory");
    fs::create_dir_all(&local_dir).expect("create local run directory");
    let mut ci = command_with_flags(
        &ci_dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &[
            "--run-id",
            "inspect-ci",
            "--label",
            "pipeline=ci",
            "--label",
            "lane=test",
        ],
        // Both runs must still be live at the *second* aggregate `inspect` below, which
        // is two full snapshot round trips into this test — hence the wider window.
        twice_inspectable_child(),
    )
    .spawn()
    .expect("spawn CI run");
    let mut local = command_with_flags(
        &local_dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "inspect-local", "--label", "pipeline=local"],
        twice_inspectable_child(),
    )
    .spawn()
    .expect("spawn local run");
    wait_until(|| record_count(&registry) == 2, RECORD_WAIT);

    let out = Command::new(bin())
        .args([
            "inspect",
            "--all",
            "--json",
            "--label",
            "pipeline=ci",
            "--label",
            "lane=test",
        ])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", &registry)
        .output()
        .expect("run aggregate inspect");
    assert_eq!(
        out.status.code(),
        Some(0),
        "aggregate inspect succeeds: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("aggregate inspect prints one JSON array");
    let outcomes = report.as_array().expect("report is an array");
    assert_eq!(outcomes.len(), 1, "only the conjunctive match is inspected");
    assert_eq!(outcomes[0]["run_id"], "inspect-ci");
    assert_eq!(outcomes[0]["status"], "inspected");
    assert_eq!(outcomes[0]["snapshot"]["run_id"], "inspect-ci");
    assert!(outcomes[0]["error"].is_null());

    let human = Command::new(bin())
        .args([
            "inspect",
            "--all",
            "--label",
            "pipeline=ci",
            "--label",
            "lane=test",
        ])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", &registry)
        .output()
        .expect("run human-readable aggregate inspect");
    assert_eq!(
        human.status.code(),
        Some(0),
        "human aggregate inspect succeeds: {}",
        String::from_utf8_lossy(&human.stderr)
    );
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("RUN_ID") && text.contains("STATUS") && text.contains("inspect-ci"),
        "the summary table identifies the target: {text}"
    );
    assert!(
        text.contains("snapshot for inspect-ci:")
            && text.contains("snapshot_version:")
            && text.contains("members:"),
        "the inspected target reuses the detailed snapshot rendering: {text}"
    );
    assert!(
        !text.contains("inspect-local"),
        "the same conjunctive filters apply in human mode: {text}"
    );

    let _ = ci.wait();
    let _ = local.wait();
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

    wait_until(|| record_count(&registry) == 2, RECORD_WAIT);

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
    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);

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

    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);

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
    wait_until(|| record_count(&registry) == 2, RECORD_WAIT);

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

/// `inspect`/`cancel`/`kill` are documented as read-only against the registry (like
/// `list`/`prune`) and must never mutate registry state just to look at it: running
/// any of them against a never-yet-created registry must leave the registry
/// directory absent, not create it (and re-assert owner-only permissions on it) as a
/// side effect of the lookup.
#[test]
fn control_clients_do_not_create_the_registry_directory() {
    let dir = scratch("control-no-create");
    let registry = registry_dir(&dir);
    assert!(
        !registry.exists(),
        "the scratch registry directory starts absent"
    );

    let out = inspect(&registry, "ghost");
    assert_eq!(out.status.code(), Some(103), "inspect fails as usual");
    assert!(
        !registry.exists(),
        "a read-only `inspect` must not create the registry directory as a side effect"
    );

    for verb in ["cancel", "kill"] {
        let out = control_client(&registry, verb, "ghost");
        assert_eq!(out.status.code(), Some(103), "{verb} fails as usual");
        assert!(
            !registry.exists(),
            "a read-only `{verb}` must not create the registry directory as a side effect"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

/// Run a mutating aggregate control verb (`cancel --all` / `kill --all`) against the
/// scratch registry and wait for it to finish.
fn control_all_client(registry: &Path, verb: &str) -> Output {
    Command::new(bin())
        .args([verb, "--all"])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .expect("spawn the --all control client")
}

/// `cancel --all` keys its snapshot by registry record, not by the non-unique
/// `run_id`, so two live runs sharing an id are independently reachable and both are
/// torn down. The by-`run-id` form remains ambiguous and is tested separately.
#[test]
fn cancel_all_reaches_every_duplicate_run_id_entry() {
    let dir = scratch("control-all-partial-failure");
    let registry = registry_dir(&dir);

    let mut solo = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "solo-run"],
        long_child(),
    )
    .spawn()
    .expect("spawn the resolvable runner");
    let mut dup_first = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "dup-target"],
        long_child(),
    )
    .spawn()
    .expect("spawn the first duplicate runner");
    let mut dup_second = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "dup-target"],
        long_child(),
    )
    .spawn()
    .expect("spawn the second duplicate runner");

    // All three entries are live and reachable before the aggregate verb runs.
    wait_until(|| record_count(&registry) == 3, RECORD_WAIT);

    let out = control_all_client(&registry, "cancel");
    assert_eq!(
        out.status.code(),
        Some(0),
        "record-addressed aggregate dispatch reaches every duplicate id; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("cancel --all prints a JSON report array");
    let outcomes = report.as_array().expect("the report is a JSON array");
    assert_eq!(
        outcomes.len(),
        3,
        "the report names every snapshot target: {report}"
    );

    let solo_outcome = outcomes
        .iter()
        .find(|entry| entry["run_id"] == "solo-run")
        .unwrap_or_else(|| panic!("the report names `solo-run`: {report}"));
    assert_eq!(
        solo_outcome["accepted"], true,
        "the resolvable target is accepted: {report}"
    );
    assert!(
        solo_outcome.get("error").is_none() || solo_outcome["error"].is_null(),
        "a successful outcome carries no error: {report}"
    );

    let dup_outcomes: Vec<&serde_json::Value> = outcomes
        .iter()
        .filter(|entry| entry["run_id"] == "dup-target")
        .collect();
    assert_eq!(
        dup_outcomes.len(),
        2,
        "both duplicate-id entries are individually named in the report: {report}"
    );
    for outcome in &dup_outcomes {
        assert_eq!(
            outcome["accepted"], true,
            "each duplicate-id record is independently accepted: {report}"
        );
        assert!(
            outcome.get("error").is_none() || outcome["error"].is_null(),
            "a successful duplicate target carries no error: {report}"
        );
        assert_eq!(outcome["status"], "accepted");
    }

    for (name, child) in [
        ("solo-run", &mut solo),
        ("dup-target first", &mut dup_first),
        ("dup-target second", &mut dup_second),
    ] {
        let status = child.wait().expect("the aggregate target exits");
        assert_eq!(
            status.code(),
            Some(108),
            "{name} receives the real control-plane cancel"
        );
    }
    assert_eq!(record_count(&registry), 0, "all three records are removed");
    let _ = fs::remove_dir_all(&dir);
}

/// The record file of the sole registered run named `run_id`, for fixtures that need
/// to edit one specific record of several while its runner stays live.
fn record_path_for(dir: &Path, run_id: &str) -> PathBuf {
    let mut matches: Vec<PathBuf> = fs::read_dir(dir)
        .expect("the registry directory exists once the runs have started")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .filter(|path| {
            let text = fs::read_to_string(path).unwrap_or_default();
            let record: serde_json::Value = match serde_json::from_str(&text) {
                Ok(record) => record,
                Err(_) => return false,
            };
            record["run_id"] == run_id
        })
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one registry record for `{run_id}`"
    );
    matches.pop().expect("checked for exactly one match")
}

/// (T-291, [K-090]) The aggregate **partial-failure** contract, end to end through the
/// binary and in the default `cargo test` tier: when part of the fan-out cannot be
/// reached, `cancel --all` still dispatches to every other target, still prints the
/// full per-run report, and *then* fails with the reserved `CONTROL` (103) code — never
/// a silent `0`, and never abandoning the reachable targets.
///
/// The unreachable target is forced deterministically rather than raced: its record's
/// advertised `endpoint` is rewritten to text that is not a well-formed control
/// endpoint on either platform, while its runner stays live and registered. That is
/// the case the shared per-target ladder (`dispatch_snapshot_target`,
/// `src/control/mod.rs`) must classify as `failed` rather than `already_gone` — the
/// record-specific re-probe still finds the entry live, so nothing confirms the run
/// ended. Its unit-level counterparts live in `src/control/tests.rs`
/// (`live_but_unreachable_target_is_a_hard_failure_for_every_aggregate_verb`); this
/// test pins what the *operator* sees: exit code, report, and stderr verdict.
#[test]
fn cancel_all_partially_failing_reports_every_target_and_the_control_code() {
    let dir = scratch("control-all-partial-failure-code");
    let registry = registry_dir(&dir);

    let mut reachable = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "reachable-run"],
        long_child(),
    )
    .spawn()
    .expect("spawn the reachable runner");
    let mut unreachable = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "unreachable-run"],
        long_child(),
    )
    .spawn()
    .expect("spawn the runner whose endpoint will be broken");
    wait_until(|| record_count(&registry) == 2, RECORD_WAIT);

    // Break exactly one target's published endpoint. The runner itself writes its
    // record once, at registration, so this edit stands for the rest of the run: the
    // entry stays `live` (its liveness lock is untouched) while its advertised
    // endpoint can no longer be connected to.
    let broken = record_path_for(&registry, "unreachable-run");
    let mut record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&broken).expect("read the target record"))
            .expect("the record is JSON");
    record["endpoint"] = serde_json::Value::String("not-a-control-endpoint".to_string());
    fs::write(
        &broken,
        serde_json::to_vec(&record).expect("serialize the edited record"),
    )
    .expect("publish the unreachable endpoint");

    let out = control_all_client(&registry, "cancel");
    assert_eq!(
        out.status.code(),
        Some(103),
        "a partially failing fan-out ends with the reserved CONTROL code; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&out.stdout)
        .expect("cancel --all prints its report even on failure");
    let outcomes = report.as_array().expect("the report is a JSON array");
    assert_eq!(
        outcomes.len(),
        2,
        "the report names every snapshot target, failed ones included: {report}"
    );

    let reached = outcomes
        .iter()
        .find(|entry| entry["run_id"] == "reachable-run")
        .unwrap_or_else(|| panic!("the report names `reachable-run`: {report}"));
    assert_eq!(
        reached["status"], "accepted",
        "one target failing must not abandon the others: {report}"
    );
    assert_eq!(reached["accepted"], true, "{report}");

    let refused = outcomes
        .iter()
        .find(|entry| entry["run_id"] == "unreachable-run")
        .unwrap_or_else(|| panic!("the report names `unreachable-run`: {report}"));
    assert_eq!(
        refused["status"], "failed",
        "a still-live target that cannot be reached is `failed`, not `already_gone`: {report}"
    );
    assert_eq!(refused["accepted"], false, "{report}");
    assert_eq!(
        refused["error"],
        "cannot cancel run `unreachable-run`: the registry entry contains an invalid control \
         endpoint",
        "the per-run reason names the run and why it could not be reached: {report}"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cancel --all: 1 of 2 target run(s)"),
        "the aggregate verdict counts the failures: {stderr}"
    );

    let status = reachable.wait().expect("the reachable target exits");
    assert_eq!(
        status.code(),
        Some(108),
        "the reachable target still received the real control-plane cancel"
    );
    // The unreachable target never got the verb — by design — so this test owns its
    // teardown.
    let _ = unreachable.kill();
    let _ = unreachable.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// `cancel --all` / `kill --all` against a registry with no confirmed-live entries —
/// an absent directory, or one holding only a confirmed-stale leftover — is not an
/// error: an empty snapshot means nothing to act on, mirroring `prune` exactly as the
/// criteria in `.work/tasks/T-217/task.md` require, and the same "empty target set is
/// not a failure" contract `wait_all_returns_at_once_with_no_live_runs` already proves
/// for `wait --all` (R-01).
#[test]
fn cancel_all_and_kill_all_return_at_once_with_no_live_runs() {
    for verb in ["cancel", "kill"] {
        let dir = scratch(&format!("control-all-empty-{verb}"));
        let registry = registry_dir(&dir);

        // A never-created registry: no live runs, trivially.
        let out = control_all_client(&registry, verb);
        assert_eq!(
            out.status.code(),
            Some(0),
            "an empty (missing) registry has nothing for {verb} --all to act on; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "[]",
            "{verb} --all prints an empty report array, not nothing and not an error"
        );
        assert_eq!(
            record_count(&registry),
            0,
            "{verb} --all against an empty registry must not create the registry directory"
        );

        // A registry with only a confirmed-stale leftover is likewise nothing to act on.
        write_stale_entry(&registry, "run-stale-0000", "run-stale-0000");
        let out = control_all_client(&registry, verb);
        assert_eq!(
            out.status.code(),
            Some(0),
            "a registry with only stale entries has nothing live for {verb} --all; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "[]",
            "{verb} --all against an all-stale registry prints an empty report array"
        );
        assert_eq!(
            record_count(&registry),
            1,
            "{verb} --all must not touch a stale entry it never acts on"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}

/// Run `list [--json]` against `registry` and wait for it to finish.
fn list(registry: &Path, json: bool) -> Output {
    let mut flags = Vec::new();
    if json {
        flags.push("--json");
    }
    list_with_flags(registry, &flags)
}

fn list_with_flags(registry: &Path, flags: &[&str]) -> Output {
    let mut cmd = Command::new(bin());
    cmd.arg("list");
    cmd.args(flags);
    cmd.env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .expect("spawn the list client")
}

#[test]
fn list_filters_by_labels_and_health_with_shared_semantics() {
    let dir = scratch("list-filters");
    let registry = registry_dir(&dir);
    let ci_dir = dir.join("ci");
    let local_dir = dir.join("local");
    fs::create_dir_all(&ci_dir).expect("create CI run scratch directory");
    fs::create_dir_all(&local_dir).expect("create local run scratch directory");
    let mut ci = command_with_flags(
        &ci_dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &[
            "--run-id",
            "list-ci",
            "--label",
            "pipeline=ci",
            "--label",
            "lane=test",
        ],
        // Both runs must stay live across two whole `list` client invocations (plus
        // the stale fixture written between them), so they use the two-invocation
        // window, not the single-observation one.
        twice_inspectable_child(),
    )
    .spawn()
    .expect("spawn labeled CI run");
    let mut local = command_with_flags(
        &local_dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "list-local", "--label", "pipeline=local"],
        twice_inspectable_child(),
    )
    .spawn()
    .expect("spawn labeled local run");
    wait_until(|| record_count(&registry) == 2, RECORD_WAIT);
    write_stale_entry(&registry, "list-stale", "list-stale");

    let out = list_with_flags(
        &registry,
        &["--json", "--label", "pipeline=ci", "--health", "live"],
    );
    assert_eq!(out.status.code(), Some(0));
    let entries: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("filtered list JSON"))
        .collect();
    assert_eq!(entries.len(), 1, "only the conjunctive live match remains");
    assert_eq!(entries[0]["run_id"], "list-ci");

    let out = list_with_flags(&registry, &["--json", "--health", "stale"]);
    let stale: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("stale list JSON"))
        .collect();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0]["run_id"], "list-stale");

    let _ = ci.wait();
    let _ = local.wait();
    let _ = fs::remove_dir_all(&dir);
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
        // The run has to still be live for the human-readable `list` further down, a
        // second whole client invocation after the `--json` one.
        twice_inspectable_child(),
    )
    .spawn()
    .expect("spawn the runner");

    // The run is listable once its record (and thus its endpoint) is published.
    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);

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
    let jsonl = entry["jsonl"]
        .as_str()
        .unwrap_or_else(|| panic!("a live run publishes its JSONL locator: {entry}"));
    assert!(
        std::path::Path::new(jsonl).is_absolute(),
        "the list JSONL locator is absolute: {jsonl}"
    );
    assert!(entry["capture_dir"].is_null());
    // T-215, end to end through the real binary: `run` publishes the redaction-safe
    // command fields into the record, and `list --json` reports them at full
    // precision. This is the only place the whole producer→record→client chain is
    // exercised as a user runs it; the unit tests cover each link on its own.
    let fingerprint = entry["argv_sha256"]
        .as_str()
        .unwrap_or_else(|| panic!("a live run's entry carries its argv fingerprint: {entry}"));
    assert!(
        fingerprint.len() == 64
            && fingerprint
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "the fingerprint is a full lowercase-hex SHA-256 digest: {fingerprint}"
    );
    assert!(
        entry["hint"].is_null(),
        "the test child is no recognized worker shape, so its hint is null: {entry}"
    );

    // The human-readable form also names the run and its health.
    let out = list(&registry, false);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("list-me") && stdout.contains("live"),
        "the human-readable form names the run and its health: {stdout}"
    );
    // …and shows the fingerprint's leading characters under its own column, so an
    // operator choosing between several live runs can tell them apart right here.
    let header = stdout
        .lines()
        .find(|line| line.starts_with("RUN_ID"))
        .unwrap_or_else(|| panic!("the human-readable table prints a header row: {stdout}"));
    let fingerprint_col = header
        .find("ARGV_SHA256")
        .unwrap_or_else(|| panic!("the header names an ARGV_SHA256 column: {header}"));
    let row = stdout
        .lines()
        .find(|line| line.contains("list-me"))
        .unwrap_or_else(|| panic!("the table names the run: {stdout}"));
    assert!(
        row[fingerprint_col..].starts_with(&fingerprint[..12]),
        "the run's ARGV_SHA256 cell abbreviates its own fingerprint: {row}"
    );

    // Let the run finish cleanly (removing its own entry).
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// T-206, end to end through the real binary: an entry whose liveness could not be
/// probed at all (its lock file resolves to a directory — see
/// [`write_unprobeable_entry`]) must print as `"unprobed"`, in both `--json` and the
/// human-readable table — never the confirmed-dead `"stale"`, which the probe never
/// actually established. A confirmed-stale sibling in the same registry still prints
/// `"stale"` as before, proving the two are not conflated in either direction.
#[test]
fn list_reports_an_unprobeable_entry_as_unprobed_not_stale() {
    let dir = scratch("list-unprobed");
    let registry = registry_dir(&dir);

    write_unprobeable_entry(&registry, "run-unprobed-0000", "run-unprobed-0000");
    write_stale_entry(&registry, "run-stale-0000", "run-stale-0000");

    let out = list(&registry, true);
    assert_eq!(
        out.status.code(),
        Some(0),
        "listing a registry with an unprobeable entry still succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 2, "both entries are listed: {stdout}");

    let entries: Vec<serde_json::Value> = lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("list --json prints valid JSON per entry"))
        .collect();
    let unprobed = entries
        .iter()
        .find(|entry| entry["run_id"] == "run-unprobed-0000")
        .expect("the unprobeable entry is listed, not dropped");
    assert_eq!(
        unprobed["health"], "unprobed",
        "an unprobeable entry must never print as the confirmed-dead 'stale': {unprobed}"
    );
    let stale = entries
        .iter()
        .find(|entry| entry["run_id"] == "run-stale-0000")
        .expect("the confirmed-stale entry is listed too");
    assert_eq!(
        stale["health"], "stale",
        "a confirmed-stale entry still prints 'stale', unaffected by the unprobed sibling: {stale}"
    );
    // Both fixtures are written in the pre-T-215 record shape — no `argv_sha256`,
    // no `hint` key at all — so this doubles as the end-to-end backward-compatibility
    // check: such a record is listed exactly as before, with the two additive fields
    // reported as `null` rather than making the record unreadable.
    for entry in &entries {
        assert!(
            entry["argv_sha256"].is_null() && entry["hint"].is_null(),
            "a record written before the command fields existed lists them as null: {entry}"
        );
    }

    // The human-readable table renders the same distinct value, not "stale". Both
    // run_ids under test (`run-unprobed-0000`, `run-stale-0000`) contain the very
    // health-word substring they are meant to distinguish, so a bare
    // `row.contains("unprobed")` / `row.contains("stale")` would pass no matter what
    // the HEALTH column actually printed. Instead, locate the HEALTH column by its
    // header offset — the same technique
    // `render_table_lines_pads_every_column_to_its_widest_value` in src/list.rs uses
    // — and assert the actual cell content.
    let out = list(&registry, false);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let header = stdout
        .lines()
        .find(|line| line.starts_with("RUN_ID"))
        .unwrap_or_else(|| panic!("the human-readable table prints a header row: {stdout}"));
    let health_col = header
        .find("HEALTH")
        .unwrap_or_else(|| panic!("the header names a HEALTH column: {header}"));

    let unprobed_row = stdout
        .lines()
        .find(|line| line.contains("run-unprobed-0000"))
        .unwrap_or_else(|| panic!("the human-readable table names the unprobeable run: {stdout}"));
    assert!(
        unprobed_row[health_col..].starts_with("unprobed"),
        "the unprobeable run's HEALTH cell reads 'unprobed', never the confirmed-dead 'stale': {unprobed_row}"
    );

    // Symmetrically, the confirmed-stale sibling's own HEALTH cell still reads
    // "stale" in the human-readable form too — this table has not been checked for
    // it before.
    let stale_row = stdout
        .lines()
        .find(|line| line.contains("run-stale-0000"))
        .unwrap_or_else(|| {
            panic!("the human-readable table names the confirmed-stale run: {stdout}")
        });
    assert!(
        stale_row[health_col..].starts_with("stale"),
        "the confirmed-stale run's HEALTH cell still reads 'stale' in the table: {stale_row}"
    );

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

/// Run `prune [--json]` against `registry` and wait for it to finish.
fn prune(registry: &Path, json: bool) -> Output {
    prune_with(registry, json, false, &[])
}

fn prune_with(registry: &Path, json: bool, dry_run: bool, labels: &[&str]) -> Output {
    let mut cmd = Command::new(bin());
    cmd.arg("prune");
    if dry_run {
        cmd.arg("--dry-run");
    }
    if json {
        cmd.arg("--json");
    }
    for label in labels {
        cmd.args(["--label", label]);
    }
    cmd.env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .expect("spawn the prune client")
}

/// Spawn `wait --run-id <id> [--timeout <duration>]` against `registry` **without**
/// waiting for it, so a test can observe whether the client is still blocked while the
/// run under test is live — the only way to prove blocking rather than infer it from
/// elapsed time. stdout/stderr are piped and read once the client is reaped.
fn spawn_wait(registry: &Path, run_id: &str, timeout: Option<&str>) -> Child {
    let mut cmd = Command::new(bin());
    cmd.args(["wait", "--run-id", run_id]);
    if let Some(timeout) = timeout {
        cmd.args(["--timeout", timeout]);
    }
    cmd.env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the wait client")
}

/// Run `wait` to completion against `registry` and hand back its output.
fn wait_for_run(registry: &Path, run_id: &str, timeout: Option<&str>) -> Output {
    spawn_wait(registry, run_id, timeout)
        .wait_with_output()
        .expect("the wait client exits")
}

fn wait_for_run_outcome(registry: &Path, run_id: &str) -> Output {
    Command::new(bin())
        .args(["wait", "--run-id", run_id, "--report-outcome"])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .expect("the reporting wait client exits")
}

/// The core `wait` contract, proved by *observation* rather than by timing: while the
/// target run is live the client is still running (checked repeatedly, against a run
/// confirmed live through its own registry record at the same moment), and once the run
/// finishes the client returns `0`.
///
/// The "still blocked" half is what makes this test non-vacuous: a `wait` that returned
/// immediately — the obvious way for this feature to be silently broken — would be
/// caught by the `try_wait` probes below, not merely produce a suspiciously short
/// elapsed time. The differential companion is
/// `wait_returns_at_once_for_a_stale_entry`: the same client against a *non*-live entry
/// returns straight away, so blocking here is caused by the run's liveness rather than
/// by `wait` always blocking.
#[test]
fn wait_blocks_until_a_live_run_finishes() {
    let dir = scratch("wait-live");
    let registry = registry_dir(&dir);
    let mut runner = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "wait-me"],
        inspectable_child(),
    )
    .spawn()
    .expect("spawn the runner");

    // The run is waitable once its record is published.
    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);

    let mut waiter = spawn_wait(&registry, "wait-me", None);

    // While the run is live the waiter must still be blocked. Probed several times
    // over ~1s, each time cross-checked against the registry actually still holding
    // the live record — so a passing probe can never mean "the run was already over".
    for _ in 0..4 {
        sleep(Duration::from_millis(250));
        if record_count(&registry) == 0 {
            panic!("the run ended sooner than this fixture expects; test is inconclusive");
        }
        assert!(
            waiter.try_wait().expect("poll the wait client").is_none(),
            "`wait` must still be blocked while the run is live"
        );
    }

    // Let the run finish on its own (a clean exit removes its entry)...
    let status = runner.wait().expect("the runner exits");
    assert!(status.success(), "the fixture run exits cleanly");

    // ...and the waiter must then return promptly, with success and no output.
    let out = waiter
        .wait_with_output()
        .expect("the wait client exits once the run is over");
    assert_eq!(
        out.status.code(),
        Some(0),
        "waiting for a run that finished exits 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "`wait` prints nothing on success — the exit code is the answer: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn wait_report_outcome_reads_the_observed_runs_terminal_event() {
    let dir = scratch("wait-reported-outcome");
    let registry = registry_dir(&dir);
    let mut runner = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "wait-outcome"],
        inspectable_child(),
    )
    .spawn()
    .expect("spawn the runner");

    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);
    let waiter = std::thread::spawn({
        let registry = registry.clone();
        move || wait_for_run_outcome(&registry, "wait-outcome")
    });
    let status = runner.wait().expect("the runner exits");
    assert!(status.success());

    let out = waiter.join().expect("the waiter thread completes");
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("one JSON outcome line");
    assert_eq!(report["run_id"], "wait-outcome");
    assert_eq!(report["status"], "reported");
    assert_eq!(report["code"], 0);
    assert_eq!(report["source"], "child_exit");
    assert_eq!(report["child_code"], 0);

    let _ = fs::remove_dir_all(&dir);
}

/// A stale entry (a leftover `.json`/`.lock` pair from a runner that died abruptly, with
/// nobody holding the lock) is not a run to wait for: `wait` classifies it through the
/// same liveness probe every other client uses and returns at once.
///
/// The `--timeout` is what makes this a real assertion rather than a timing guess: the
/// stale fixture never disappears, so an implementation that mistook it for live could
/// only ever exit `WAIT_TIMEOUT` (112) here. Exiting `0` therefore proves the entry was
/// actively classified as not-live — and, together with
/// `wait_blocks_until_a_live_run_finishes`, that `wait` blocks on liveness rather than
/// on nothing at all.
#[test]
fn wait_returns_at_once_for_a_stale_entry() {
    let dir = scratch("wait-stale");
    let registry = registry_dir(&dir);
    write_stale_entry(&registry, "run-stale-0000", "run-stale-0000");

    let out = wait_for_run(&registry, "run-stale-0000", Some("10s"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "a stale entry means the run is over, so wait succeeds instead of timing out; \
         stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `wait` is read-only: classifying the entry must not reap it (that is `prune`'s
    // job) or otherwise disturb the registry.
    assert!(
        registry.join("run-stale-0000.json").exists()
            && registry.join("run-stale-0000.lock").exists(),
        "a read-only wait must leave the stale entry's files on disk"
    );

    // The same immediate `0` for a run id that was never registered at all — the
    // documented conflation with "already finished and cleaned up" (a clean exit
    // deletes its own record, so the two are the same observation).
    let out = wait_for_run(&registry, "never-registered", Some("10s"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "an unknown run id reads as finished, not as an error; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let reported = wait_for_run_outcome(&registry, "never-registered");
    assert_eq!(reported.status.code(), Some(0));
    let report: serde_json::Value =
        serde_json::from_slice(&reported.stdout).expect("one unknown outcome line");
    assert_eq!(report["status"], "unknown");
    assert!(report["code"].is_null());
    assert!(report["source"].is_null());
    assert!(report["child_code"].is_null());

    let _ = fs::remove_dir_all(&dir);
}

/// The waiter's **own** deadline: `--timeout` elapsing on a live, non-finishing run is
/// reported with the reserved `WAIT_TIMEOUT` code (112) — not `0` (which would claim the
/// run is over), not the run's own `TIMEOUT` (106), and not `CONTROL` (103) (nothing was
/// unreachable). The run itself is untouched by the give-up and is still live afterwards.
#[test]
fn wait_times_out_on_a_live_run_with_its_own_reserved_code() {
    let dir = scratch("wait-timeout");
    let registry = registry_dir(&dir);
    let mut runner = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "long-runner"],
        long_child(),
    )
    .spawn()
    .expect("spawn the runner");

    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);

    let out = wait_for_run(&registry, "long-runner", Some("1s"));
    assert_eq!(
        out.status.code(),
        Some(112),
        "a waiter's own deadline exits with the reserved WAIT_TIMEOUT code; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "a timed-out wait prints nothing on stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("long-runner"),
        "the failure names the run: {stderr}"
    );
    assert!(
        stderr.contains("still live"),
        "the failure says the run outlived the wait, not that it was stopped: {stderr}"
    );

    // The give-up left the run completely alone — it is still registered and running.
    assert_eq!(
        record_count(&registry),
        1,
        "a wait that gave up must not have ended the run it was waiting for"
    );
    assert!(
        runner.try_wait().expect("poll the runner").is_none(),
        "the run is still going after the waiter gave up"
    );

    let _ = runner.kill();
    let _ = runner.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// Two concurrent live runs under one `--run-id` leave no single run to wait for, so
/// `wait` fails closed with the same `CONTROL` (103) "ambiguous run id" verdict
/// `inspect`/`cancel`/`kill` give — rather than silently tracking whichever entry the
/// directory scan happened to return first. Neither run is disturbed.
#[test]
fn wait_reports_ambiguous_run_id_for_duplicate_run_ids() {
    let dir = scratch("wait-ambiguous");
    let registry = registry_dir(&dir);
    let run_id = "dup-wait";

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

    wait_until(|| record_count(&registry) == 2, RECORD_WAIT);

    // A generous `--timeout` that must never be reached: the ambiguity is a hard
    // failure detected on the very first probe, so a 112 here would mean `wait` sat on
    // an ambiguous id instead of refusing it.
    let out = wait_for_run(&registry, run_id, Some("30s"));
    assert_eq!(
        out.status.code(),
        Some(103),
        "an ambiguous run id is a CONTROL failure for wait too; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ambiguous"),
        "the wait failure names the reason: {stderr}"
    );
    assert!(
        stderr.contains(run_id),
        "the wait failure names the run: {stderr}"
    );

    // The rejected command touched neither run.
    assert_eq!(
        record_count(&registry),
        2,
        "a rejected ambiguous wait must not end either run"
    );

    let _ = first.kill();
    let _ = first.wait();
    let _ = second.kill();
    let _ = second.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// `wait` is documented as read-only and must never mutate registry state just to look:
/// waiting on a never-yet-created registry must leave the directory absent (and, since
/// an absent registry holds no live run, return `0` at once).
#[test]
fn wait_does_not_create_the_registry_directory() {
    let dir = scratch("wait-no-create");
    let registry = registry_dir(&dir);
    assert!(
        !registry.exists(),
        "the scratch registry directory starts absent"
    );

    let out = wait_for_run(&registry, "ghost", Some("10s"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "a missing registry holds no live run, so the wait is already over; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !registry.exists(),
        "a read-only `wait` must not create the registry directory as a side effect"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Spawn `wait --all [--timeout <duration>]` against `registry` **without** waiting
/// for it, mirroring `spawn_wait` for the aggregate barrier: the only way to prove
/// blocking (or the deliberate absence of it, T-216) rather than infer it from
/// elapsed time.
fn spawn_wait_all(registry: &Path, timeout: Option<&str>) -> Child {
    let mut cmd = Command::new(bin());
    cmd.args(["wait", "--all"]);
    if let Some(timeout) = timeout {
        cmd.args(["--timeout", timeout]);
    }
    cmd.env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the wait --all client")
}

/// Run `wait --all` to completion against `registry` and hand back its output.
fn wait_all_for(registry: &Path, timeout: Option<&str>) -> Output {
    spawn_wait_all(registry, timeout)
        .wait_with_output()
        .expect("the wait --all client exits")
}

/// `wait --all` against a registry with no live runs at all — an absent directory, or
/// one holding only confirmed-stale leftovers — succeeds at once: the snapshot's
/// target set is already empty, so there is nothing to wait for.
///
/// The generous `--timeout` makes this a real assertion rather than a timing guess,
/// exactly like `wait_returns_at_once_for_a_stale_entry`: an implementation that
/// mistook a stale entry for live could only ever exit `WAIT_TIMEOUT` (112) here.
#[test]
fn wait_all_returns_at_once_with_no_live_runs() {
    let dir = scratch("wait-all-empty");
    let registry = registry_dir(&dir);

    // A never-created registry: no live runs, trivially.
    let out = wait_all_for(&registry, Some("10s"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "an empty (missing) registry has nothing to wait for; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "`wait --all` prints nothing on success: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    // A registry with only confirmed-stale leftovers is likewise nothing to wait for.
    write_stale_entry(&registry, "run-stale-0000", "run-stale-0000");
    let out = wait_all_for(&registry, Some("10s"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "a registry with only stale entries has nothing live to wait for; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The core `--all` contract, proved by observation like
/// `wait_blocks_until_a_live_run_finishes`: while the one snapshot-confirmed run is
/// live the client stays blocked, and once that run finishes `wait --all` returns `0`.
#[test]
fn wait_all_blocks_until_the_one_live_run_finishes() {
    let dir = scratch("wait-all-live");
    let registry = registry_dir(&dir);
    let mut runner = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "wait-all-me"],
        inspectable_child(),
    )
    .spawn()
    .expect("spawn the runner");

    // The run is waitable once its record is published.
    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);

    let mut waiter = spawn_wait_all(&registry, None);

    // While the run is live the waiter must still be blocked, cross-checked against
    // the registry actually still holding the live record on every probe.
    for _ in 0..4 {
        sleep(Duration::from_millis(250));
        if record_count(&registry) == 0 {
            panic!("the run ended sooner than this fixture expects; test is inconclusive");
        }
        assert!(
            waiter
                .try_wait()
                .expect("poll the wait --all client")
                .is_none(),
            "`wait --all` must still be blocked while its snapshot-confirmed run is live"
        );
    }

    // Let the run finish on its own (a clean exit removes its entry)...
    let status = runner.wait().expect("the runner exits");
    assert!(status.success(), "the fixture run exits cleanly");

    // ...and the waiter must then return promptly, with success and no output.
    let out = waiter
        .wait_with_output()
        .expect("the wait --all client exits once its snapshot is clear");
    assert_eq!(
        out.status.code(),
        Some(0),
        "waiting for every snapshot run to finish exits 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "`wait --all` prints nothing on success: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The waiter's own deadline, aggregate flavor: `--timeout` elapsing while a
/// snapshot-confirmed run is still live reports `WAIT_TIMEOUT` (112), names how many
/// snapshot entries are still outstanding, and leaves the run completely
/// untouched — the aggregate counterpart to
/// `wait_times_out_on_a_live_run_with_its_own_reserved_code`.
#[test]
fn wait_all_times_out_with_a_run_still_live() {
    let dir = scratch("wait-all-timeout");
    let registry = registry_dir(&dir);
    let mut runner = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "long-all-runner"],
        long_child(),
    )
    .spawn()
    .expect("spawn the runner");

    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);

    let out = wait_all_for(&registry, Some("1s"));
    assert_eq!(
        out.status.code(),
        Some(112),
        "the waiter's own deadline exits with the reserved WAIT_TIMEOUT code; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "a timed-out `wait --all` prints nothing on stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains('1') && stderr.contains("still outstanding"),
        "the failure names how many snapshot entries are still outstanding: {stderr}"
    );
    assert!(
        stderr.contains("still live"),
        "the failure says the outstanding run(s) outlived the wait, not that they were \
         stopped: {stderr}"
    );

    // The give-up left the run completely alone — it is still registered and running.
    assert_eq!(
        record_count(&registry),
        1,
        "a wait --all that gave up must not have ended the run it was waiting for"
    );
    assert!(
        runner.try_wait().expect("poll the runner").is_none(),
        "the run is still going after the waiter gave up"
    );

    let _ = runner.kill();
    let _ = runner.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// **Snapshot semantics.** A run that registers *after* `wait --all`'s snapshot is out
/// of scope for that invocation — documented in `src/wait.rs`'s module doc and
/// `docs/registry.md`, "Waiting — `wait`" — and this proves it by observation rather
/// than by reading the doc comment. `run1` is confirmed live *before* `wait --all`
/// starts, so it is inside the snapshot; `run2` is spawned — and only confirmed live —
/// well after `wait --all` has had time to take its snapshot, so it is provably
/// outside it. Once `run1` alone finishes, `wait --all` must return `0` even though
/// `run2` is still live and registered.
#[test]
fn wait_all_snapshot_excludes_a_run_that_registers_after_it_starts() {
    let dir = scratch("wait-all-snapshot");
    let registry = registry_dir(&dir);

    let mut run1 = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "snapshot-run1"],
        inspectable_child(),
    )
    .spawn()
    .expect("spawn the first runner");
    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);

    let waiter = spawn_wait_all(&registry, None);
    // Give the waiter comfortable room to take its one snapshot scan before the
    // second run is ever spawned, let alone registered.
    sleep(Duration::from_millis(500));

    let mut run2 = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "snapshot-run2"],
        long_child(),
    )
    .spawn()
    .expect("spawn the second runner");
    wait_until(|| record_count(&registry) == 2, RECORD_WAIT);

    // `run1` finishing on its own clears everything the snapshot actually contained.
    let status = run1.wait().expect("the first runner exits");
    assert!(status.success(), "the first fixture run exits cleanly");

    // `run2` is still live and registered, but it is not part of this invocation's
    // snapshot, so the waiter must return promptly rather than block on it.
    let out = waiter
        .wait_with_output()
        .expect("the wait --all client exits once its snapshot is clear");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a run outside the snapshot must never be waited for; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        record_count(&registry),
        1,
        "run2 must still be registered and live after the waiter returned"
    );

    let _ = run2.kill();
    let _ = run2.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// R-02's documented asymmetry, proved end to end: a registry holding **only** an
/// unprobeable entry (no confirmed-live one at all — see [`write_unprobeable_entry`])
/// makes `wait --all` return `0` immediately, exactly like an empty registry, because
/// the snapshot's target set is exactly the entries *confirmed* `Health::Live` at that
/// instant — an entry that is only unprobed then is excluded outright, never entering
/// the target set. This is deliberately **not** the same "unknown is not confirmed"
/// leniency `--run-id` and a *later* pass over an already-tracked entry both give an
/// unprobeable record; see `snapshot_live_targets_includes_only_confirmed_live_entries`
/// in `src/wait.rs` for the unit-level proof of the same rule.
#[test]
fn wait_all_returns_at_once_when_the_only_entry_is_unprobed_at_snapshot_time() {
    let dir = scratch("wait-all-unprobed-only");
    let registry = registry_dir(&dir);

    write_unprobeable_entry(&registry, "run-unprobed-0000", "run-unprobed-0000");

    let out = wait_all_for(&registry, Some("10s"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "an entry unprobed at snapshot time is excluded from the target set outright, \
         so there is nothing to wait for; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "`wait --all` prints nothing on success: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The other half of the same asymmetry: an unprobeable entry sitting **alongside** a
/// confirmed-live run at snapshot time never enters the target set either, so it must
/// not leak into the timeout diagnostic — a give-up here is worded exactly like
/// `wait_all_times_out_with_a_run_still_live`'s single-live-run case ("still live"),
/// never the `any_unprobed` wording that would wrongly claim the unprobeable sibling
/// was actually being tracked.
#[test]
fn wait_all_timeout_ignores_an_unprobed_entry_never_in_the_snapshot() {
    let dir = scratch("wait-all-unprobed-and-live");
    let registry = registry_dir(&dir);
    let mut runner = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "long-all-runner-with-unprobed-sibling"],
        long_child(),
    )
    .spawn()
    .expect("spawn the runner");

    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);
    write_unprobeable_entry(
        &registry,
        "run-unprobed-sibling-0000",
        "run-unprobed-sibling-0000",
    );

    let out = wait_all_for(&registry, Some("1s"));
    assert_eq!(
        out.status.code(),
        Some(112),
        "the live run alone still times out the waiter; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains('1') && stderr.contains("still outstanding"),
        "exactly one snapshot entry (the live run) is outstanding — the unprobed \
         sibling was never tracked: {stderr}"
    );
    assert!(
        stderr.contains("still live"),
        "the diagnostic must confidently say the tracked run is still live, since the \
         untracked unprobed sibling never contributes to `any_unprobed`: {stderr}"
    );
    assert!(
        !stderr.contains("not confirmed finished"),
        "the untracked unprobed sibling must never leak into the give-up wording: {stderr}"
    );

    let _ = runner.kill();
    let _ = runner.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// The end-to-end reaping contract: `prune` deletes a confirmed-stale entry from disk
/// while leaving a live run's entry completely untouched — the through-the-binary
/// counterpart to the fine-grained `Registry::prune` unit tests in `src/registry/mod.rs`.
#[test]
fn prune_reaps_a_stale_entry_and_keeps_a_live_one() {
    let dir = scratch("prune-mixed");
    let registry = registry_dir(&dir);

    // A hand-written, confirmed-stale entry (record + unlocked lock file).
    write_stale_entry(&registry, "run-stale-0000", "run-stale-0000");
    assert!(
        registry.join("run-stale-0000.json").exists()
            && registry.join("run-stale-0000.lock").exists(),
        "the stale fixture starts on disk"
    );

    // A real, live run alongside it — its runner holds the liveness lock for the whole
    // run, so prune must never touch it.
    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "live-run"],
        long_child(),
    )
    .spawn()
    .expect("spawn the runner");

    // Both records are present once the live runner has published its own.
    wait_until(|| record_count(&registry) == 2, RECORD_WAIT);

    let out = prune(&registry, true);
    assert_eq!(
        out.status.code(),
        Some(0),
        "prune succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("prune --json prints one JSON object");
    assert_eq!(
        report["pruned"], 1,
        "exactly the stale entry is reaped: {report}"
    );
    assert_eq!(
        report["live"], 1,
        "the live entry is counted as kept, not reaped: {report}"
    );

    // The stale entry is gone from disk, both files...
    assert!(
        !registry.join("run-stale-0000.json").exists(),
        "the stale record file is reaped"
    );
    assert!(
        !registry.join("run-stale-0000.lock").exists(),
        "the stale lock file is reaped"
    );
    // ...and only the live run's record survives.
    assert_eq!(
        record_count(&registry),
        1,
        "only the live entry's record remains"
    );
    let survivor = read_only_record(&registry);
    assert!(
        survivor.contains("live-run"),
        "the surviving record is the live run's: {survivor}"
    );

    // A second prune over the now-live-only registry reaps nothing.
    let out = prune(&registry, true);
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("prune --json prints one JSON object");
    assert_eq!(
        report["pruned"], 0,
        "a repeat prune leaves the live entry alone: {report}"
    );
    assert_eq!(
        record_count(&registry),
        1,
        "the live entry still stands after a repeat prune"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// The orphan-lock counterpart to `prune_reaps_a_stale_entry_and_keeps_a_live_one`:
/// alongside the same confirmed-stale `.json`/`.lock` pair and live run, a lone
/// `.lock` file with **no `.json` sibling** is reaped too — the leftover
/// `Registry::register`'s Drop guard now backstops at the source, but which
/// `Registry::prune`'s separate orphan-lock pass must still clean up wherever else
/// it arises (a hand-edited directory, or a `Registration::remove` whose `.json`
/// delete succeeded but whose `.lock` delete did not).
#[test]
fn prune_reaps_an_orphaned_lock_file_alongside_a_stale_pair_and_a_live_run() {
    let dir = scratch("prune-orphan-lock-mixed");
    let registry = registry_dir(&dir);

    // A hand-written, confirmed-stale entry (record + unlocked lock file).
    write_stale_entry(&registry, "run-stale-0000", "run-stale-0000");

    // A lone, unlocked `.lock` file with no `.json` sibling at all. Backdated well
    // past `Registry`'s `ORPHAN_LOCK_MIN_AGE` ([R-01]) so it reads as a confirmed,
    // long-sitting orphan rather than the brief, legitimate pre-lock window a
    // just-starting `reserve_entry` would otherwise leave the same shape in.
    let orphan_lock = registry.join("orphan-0000.lock");
    fs::write(&orphan_lock, b"").expect("write the orphaned lock file");
    backdate(&orphan_lock, Duration::from_secs(30));
    assert!(
        orphan_lock.exists() && !registry.join("orphan-0000.json").exists(),
        "the orphaned lock fixture has no paired record"
    );

    // A real, live run alongside them both.
    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "live-run"],
        long_child(),
    )
    .spawn()
    .expect("spawn the runner");

    wait_until(|| record_count(&registry) == 2, RECORD_WAIT);

    let out = prune(&registry, true);
    assert_eq!(
        out.status.code(),
        Some(0),
        "prune succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("prune --json prints one JSON object");
    assert_eq!(
        report["pruned"], 1,
        "the confirmed-stale pair is reaped: {report}"
    );
    assert_eq!(
        report["live"], 1,
        "the live entry is counted as kept, not reaped: {report}"
    );
    assert_eq!(
        report["orphaned_locks"], 1,
        "the lone orphaned lock file is reaped too: {report}"
    );

    assert!(
        !registry.join("run-stale-0000.json").exists()
            && !registry.join("run-stale-0000.lock").exists(),
        "the stale pair's files are both reaped"
    );
    assert!(
        !orphan_lock.exists(),
        "the orphaned lock file is reaped alongside the stale pair"
    );
    assert_eq!(
        record_count(&registry),
        1,
        "only the live entry's record remains"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// Run `prune --dry-run [--json]` against `registry` and wait for it to finish.
fn prune_dry_run(registry: &Path, json: bool) -> Output {
    prune_with(registry, json, true, &[])
}

#[test]
fn prune_label_filter_scopes_preview_and_reap_and_excludes_orphan_locks() {
    let dir = scratch("prune-label-filter");
    let registry = registry_dir(&dir);
    let ci_labels = BTreeMap::from([
        ("pipeline".to_string(), "ci".to_string()),
        ("lane".to_string(), "test".to_string()),
    ]);
    let local_labels = BTreeMap::from([
        ("pipeline".to_string(), "local".to_string()),
        ("lane".to_string(), "test".to_string()),
    ]);
    write_stale_entry_with_labels(&registry, "ci-stale", "ci-stale", ci_labels);
    write_stale_entry_with_labels(&registry, "local-stale", "local-stale", local_labels);

    let orphan_lock = registry.join("owner-unknown.lock");
    fs::write(&orphan_lock, b"").expect("write an ownerless orphan lock");
    backdate(&orphan_lock, Duration::from_secs(30));

    let preview = prune_with(&registry, true, true, &["pipeline=ci", "lane=test"]);
    assert_eq!(preview.status.code(), Some(0));
    let preview_report: serde_json::Value =
        serde_json::from_slice(&preview.stdout).expect("filtered dry-run JSON");
    assert_eq!(preview_report["pruned"], 1);
    assert_eq!(preview_report["orphaned_locks"], 0);
    assert_eq!(preview_report["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(preview_report["candidates"][0]["run_id"], "ci-stale");
    assert!(registry.join("ci-stale.json").exists());
    assert!(registry.join("local-stale.json").exists());
    assert!(orphan_lock.exists());

    let reaped = prune_with(&registry, true, false, &["pipeline=ci", "lane=test"]);
    assert_eq!(reaped.status.code(), Some(0));
    let reap_report: serde_json::Value =
        serde_json::from_slice(&reaped.stdout).expect("filtered prune JSON");
    assert_eq!(reap_report["pruned"], 1);
    assert_eq!(reap_report["orphaned_locks"], 0);
    assert!(!registry.join("ci-stale.json").exists());
    assert!(!registry.join("ci-stale.lock").exists());
    assert!(registry.join("local-stale.json").exists());
    assert!(registry.join("local-stale.lock").exists());
    assert!(orphan_lock.exists());

    let _ = fs::remove_dir_all(&dir);
}

/// T-199, end to end through the real binary: `prune --dry-run --json` over the
/// same mixed fixture as `prune_reaps_an_orphaned_lock_file_alongside_a_stale_pair_
/// and_a_live_run` (a confirmed-stale paired entry, a confirmed-stale orphaned lock,
/// and a live run) previews exactly what a following real `prune --json` pass
/// reaps — same aggregate counts, same candidates — while deleting nothing itself.
#[test]
fn prune_dry_run_previews_without_deleting_and_matches_a_real_prune() {
    let dir = scratch("prune-dry-run");
    let registry = registry_dir(&dir);

    // A hand-written, confirmed-stale entry (record + unlocked lock file).
    write_stale_entry(&registry, "run-stale-0000", "run-stale-0000");

    // A lone, unlocked `.lock` file with no `.json` sibling, backdated past the
    // registry's `ORPHAN_LOCK_MIN_AGE` ([R-01]) so it reads as a confirmed orphan.
    let orphan_lock = registry.join("orphan-0000.lock");
    fs::write(&orphan_lock, b"").expect("write the orphaned lock file");
    backdate(&orphan_lock, Duration::from_secs(30));

    // A real, live run alongside them both.
    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "live-run"],
        long_child(),
    )
    .spawn()
    .expect("spawn the runner");

    wait_until(|| record_count(&registry) == 2, RECORD_WAIT);

    let dry_out = prune_dry_run(&registry, true);
    assert_eq!(
        dry_out.status.code(),
        Some(0),
        "prune --dry-run succeeds; stderr: {}",
        String::from_utf8_lossy(&dry_out.stderr)
    );
    let dry_report: serde_json::Value = serde_json::from_slice(&dry_out.stdout)
        .expect("prune --dry-run --json prints one JSON object");
    assert_eq!(
        dry_report["pruned"], 1,
        "the confirmed-stale pair would be reaped: {dry_report}"
    );
    assert_eq!(
        dry_report["live"], 1,
        "the live entry would be kept, not reaped: {dry_report}"
    );
    assert_eq!(
        dry_report["orphaned_locks"], 1,
        "the lone orphaned lock file would be reaped too: {dry_report}"
    );
    assert_eq!(
        dry_report["unprobed"], 0,
        "nothing is unprobeable in this fixture: {dry_report}"
    );
    let candidates = dry_report["candidates"]
        .as_array()
        .expect("dry-run --json carries a candidates array");
    assert_eq!(
        candidates.len(),
        2,
        "one paired entry plus one orphaned lock: {dry_report}"
    );
    assert!(
        candidates.iter().any(
            |candidate| candidate["kind"] == "entry" && candidate["run_id"] == "run-stale-0000"
        ),
        "the stale paired entry appears as a candidate: {dry_report}"
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate["kind"] == "orphaned_lock"
                && candidate["lock_file_name"] == "orphan-0000.lock"),
        "the orphaned lock file appears as a candidate: {dry_report}"
    );

    // Nothing was actually deleted: the stale pair, the orphaned lock, and the live
    // pair all still stand.
    assert!(
        registry.join("run-stale-0000.json").exists()
            && registry.join("run-stale-0000.lock").exists(),
        "prune --dry-run must not delete the stale pair"
    );
    assert!(
        orphan_lock.exists(),
        "prune --dry-run must not delete the orphaned lock file"
    );
    assert_eq!(
        record_count(&registry),
        2,
        "prune --dry-run must not touch any record file"
    );

    // A following real prune reaps exactly what the dry run predicted.
    let real_out = prune(&registry, true);
    assert_eq!(
        real_out.status.code(),
        Some(0),
        "the real prune following the dry run succeeds; stderr: {}",
        String::from_utf8_lossy(&real_out.stderr)
    );
    let real_report: serde_json::Value =
        serde_json::from_slice(&real_out.stdout).expect("prune --json prints one JSON object");
    assert_eq!(
        real_report["pruned"], dry_report["pruned"],
        "the real prune's tally matches the dry run's prediction: {real_report}"
    );
    assert_eq!(
        real_report["live"], dry_report["live"],
        "the real prune's tally matches the dry run's prediction: {real_report}"
    );
    assert_eq!(
        real_report["unprobed"], dry_report["unprobed"],
        "the real prune's tally matches the dry run's prediction: {real_report}"
    );
    assert_eq!(
        real_report["orphaned_locks"], dry_report["orphaned_locks"],
        "the real prune's tally matches the dry run's prediction: {real_report}"
    );
    assert!(
        !registry.join("run-stale-0000.json").exists()
            && !registry.join("run-stale-0000.lock").exists()
            && !orphan_lock.exists(),
        "the real prune reaps exactly what the dry run predicted"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}

/// The `endpoint` the sole registry record publishes, or `None` while it has not
/// published one (yet, or at all — a run whose control transport could not be stood
/// up registers a `null` endpoint and still works).
///
/// `cfg(unix)` alongside its only caller below: on Windows an endpoint is a named
/// pipe with no filesystem leftover to look for, so nothing here reads it.
#[cfg(unix)]
fn record_endpoint(registry: &Path) -> Option<String> {
    let record: serde_json::Value =
        serde_json::from_str(&read_only_record(registry)).expect("the record is JSON");
    record["endpoint"].as_str().map(str::to_string)
}

/// T-207 end to end, through the real binary and a real abrupt death: a runner killed
/// with `SIGKILL` runs no teardown at all, so it strands **both** its registry entry
/// and the control socket it published — a `0700` `pkc-…` directory holding one
/// socket, which nothing used to clean up. One `prune` now reaps both.
///
/// Unix-only because the leak is: a Windows runner publishes a named pipe, which
/// lives in the kernel object namespace and disappears with its creator.
#[cfg(unix)]
#[test]
fn prune_reaps_the_control_socket_of_an_abruptly_killed_runner() {
    let dir = scratch("prune-socket-abrupt");
    let registry = registry_dir(&dir);

    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "socket-run"],
        long_child(),
    )
    .spawn()
    .expect("spawn the runner");

    // Wait for the record *and* the endpoint it publishes once its transport is up.
    wait_until(|| record_count(&registry) == 1, RECORD_WAIT);
    wait_until(|| record_endpoint(&registry).is_some(), RECORD_WAIT);
    let endpoint = record_endpoint(&registry).expect("the live run published an endpoint");
    let socket = PathBuf::from(&endpoint);
    let socket_dir = socket
        .parent()
        .expect("the endpoint names a socket inside its own directory")
        .to_path_buf();
    assert!(
        socket.exists() && socket_dir.is_dir(),
        "the live runner's control socket is on disk at {endpoint}"
    );

    // Abrupt death: `Child::kill` is `SIGKILL` here, so no teardown of any kind runs
    // — neither the registry entry's removal nor the control server's `Drop`.
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        socket.exists(),
        "an abruptly-killed runner leaves its control socket behind"
    );
    assert_eq!(
        record_count(&registry),
        1,
        "…and its registry record too — the leftover pair prune exists for"
    );

    let out = prune(&registry, true);
    assert_eq!(
        out.status.code(),
        Some(0),
        "prune succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("prune --json prints one JSON object");
    assert_eq!(
        report["pruned"], 1,
        "the confirmed-stale entry is reaped: {report}"
    );
    assert_eq!(
        record_count(&registry),
        0,
        "the stale record is gone from the registry"
    );
    assert!(
        !socket.exists(),
        "the control socket the dead runner published is reaped too, not leaked"
    );
    assert!(
        !socket_dir.exists(),
        "its private directory goes with it, leaving no `pkc-…` litter behind"
    );

    let _ = fs::remove_dir_all(&socket_dir);
    let _ = fs::remove_dir_all(&dir);
}
