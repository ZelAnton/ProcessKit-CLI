//! End-to-end containment tier — proves ProcessKit's teardown guarantees *through
//! the built `processkit-cli` binary*, the value this repository adds over the
//! library's own suite (`AGENTS.md`, "Testing tiers"). Heavier than the
//! through-the-binary base proofs in `tests/run.rs`: it spawns real multi-level
//! process trees, observes liveness from *outside* the runner (an OS
//! process-table probe — see [`common::pid_is_alive`] — not the container's own
//! member list), and stresses rapid PID reuse.
//!
//! Gated behind the `e2e` Cargo feature so the default `cargo test` never runs it;
//! the dedicated CI `e2e` job does (`cargo test --features e2e --test e2e`). Run
//! it locally the same way. Each scenario that needs a platform primitive the host
//! lacks skips **loudly** (a `SKIP …` line, visible under `--nocapture`) rather
//! than silently passing.
//!
//! Teardown here is by design free of kill-by-PID: leaked workers self-terminate
//! on a bounded timer (`e2e_helper`), and processes the tier owns a handle for are
//! killed through that handle ([`common::ChildGuard`]). Killing a recycled PID is
//! the exact hazard the PID-reuse scenario guards against, so the harness never
//! does it.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{
    ChildGuard, Scenario, bin, command_with_flags, events_path, file_len, helper_bin, pid_is_alive,
    read_events, read_pid, run_with_flags, shell_inline, wait_child_bounded,
    wait_for_file_nonempty, wait_until,
};

/// The `-- <program> <args…>` tail that runs the e2e helper in `mode` with `args`,
/// as the plain owned strings `command_with_flags` / `run_with_flags` accept.
fn helper(mode: &str, args: &[&str]) -> Vec<String> {
    let mut tail = Vec::with_capacity(args.len() + 2);
    tail.push(helper_bin().to_string());
    tail.push(mode.to_string());
    tail.extend(args.iter().map(|arg| arg.to_string()));
    tail
}

/// Headline guarantee: a grandchild the child leaked and abandoned does not
/// survive a **clean** root exit — observed by PID from outside the runner, not
/// merely inferred from the exit code. `run` returns only after teardown, so the
/// container has already reaped the whole tree.
#[test]
fn leaked_grandchild_is_reaped_after_a_clean_root_exit() {
    let scenario = Scenario::new("e2e-leak-clean");
    let pidfile = scenario.path("grandchild.pid");
    let pidfile_arg = pidfile.to_string_lossy().into_owned();

    let out = run_with_flags(
        &scenario.dir,
        &[],
        &[],
        helper(
            "root",
            &["--pidfile", &pidfile_arg, "--grandchild-sleep-secs", "120"],
        ),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "the root exits cleanly; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let grandchild = read_pid(&pidfile).expect("the root recorded the grandchild PID");
    assert!(
        wait_until(|| !pid_is_alive(grandchild), Duration::from_secs(10)),
        "a leaked grandchild (PID {grandchild}) survived a clean root exit — the \
         container did not reap the tree"
    );

    // Cross-check the JSONL event contract against the PID-observed teardown: the
    // stream must open with `run_started`, bracket a `cleanup_finished`, and end
    // with a `runner_exit` carrying the child's own code.
    let events = read_events(&events_path(&scenario.dir));
    let tag = |event: &serde_json::Value| event["event"].as_str().unwrap_or_default().to_string();
    assert!(
        events.iter().any(|event| tag(event) == "run_started"),
        "the run must emit run_started"
    );
    assert!(
        events.iter().any(|event| tag(event) == "cleanup_finished"),
        "the run must record teardown via cleanup_finished"
    );
    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(tag(runner_exit), "runner_exit", "runner_exit is terminal");
    assert_eq!(
        runner_exit["child_code"], 0,
        "runner_exit preserves the child's own code: {runner_exit}"
    );
}

/// Same teardown guarantee for a **nonzero** root, and the child's exact code
/// reaches the caller unclamped — proving the teardown does not hinge on the exit
/// code and the code is forwarded faithfully.
#[test]
fn leaked_grandchild_is_reaped_and_a_nonzero_code_survives_unclamped() {
    let scenario = Scenario::new("e2e-leak-nonzero");
    let pidfile = scenario.path("grandchild.pid");
    let pidfile_arg = pidfile.to_string_lossy().into_owned();

    let out = run_with_flags(
        &scenario.dir,
        &[],
        &[],
        helper(
            "root",
            &[
                "--pidfile",
                &pidfile_arg,
                "--code",
                "42",
                "--grandchild-sleep-secs",
                "120",
            ],
        ),
    );
    // 42 is a child code, well clear of the runner-own band (100..=119): it must
    // pass through verbatim, not be clamped or aliased.
    assert_eq!(
        out.status.code(),
        Some(42),
        "the child's exact nonzero code must reach the caller; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let grandchild = read_pid(&pidfile).expect("the root recorded the grandchild PID");
    assert!(
        wait_until(|| !pid_is_alive(grandchild), Duration::from_secs(10)),
        "a leaked grandchild (PID {grandchild}) survived a nonzero root exit — the \
         teardown guarantee must not depend on the exit code"
    );
}

/// Force-kill the runner while its child and grandchild are alive, then prove the
/// exact platform contract reported in `run_started`: Windows reaps the whole
/// Job, Linux's enabled PDEATHSIG kills only the direct child, and macOS/other
/// Unix currently provides no abrupt-owner-death cleanup. Unix survivors are
/// bounded helpers and self-terminate; the test never kills an observed PID.
#[test]
fn abrupt_runner_death_reports_and_enforces_platform_scope() {
    let scenario = Scenario::new("e2e-abrupt");
    let pidfile = scenario.path("grandchild.pid");
    let pidfile_arg = pidfile.to_string_lossy().into_owned();

    // A root that stays alive keeps the runner blocked in its wait, so killing the
    // runner is genuinely abrupt rather than the ordinary post-exit teardown.
    let runner = command_with_flags(
        &scenario.dir,
        &[],
        &[],
        helper(
            "root",
            &[
                "--pidfile",
                &pidfile_arg,
                "--root-sleep-secs",
                "12",
                "--grandchild-sleep-secs",
                "12",
            ],
        ),
    )
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn the runner");
    let mut runner = ChildGuard::new(runner);

    assert!(
        wait_for_file_nonempty(&pidfile, Duration::from_secs(20)),
        "the root never recorded a grandchild PID"
    );
    let grandchild = read_pid(&pidfile).expect("the root recorded the grandchild PID");
    let events = read_events(&events_path(&scenario.dir));
    let started = events
        .iter()
        .find(|event| event["event"] == "run_started")
        .expect("the runner flushed run_started before the helper wrote its pidfile");
    let root = started["root_pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .expect("run_started carries a valid root PID");
    assert!(
        pid_is_alive(root) && pid_is_alive(grandchild),
        "the root (PID {root}) and grandchild (PID {grandchild}) should both be alive before \
         the runner is killed"
    );

    // Abrupt, identity-safe kill through the Child handle: no clean shutdown and
    // no ProcessGroup::drop. From here on PID probes only observe; they never kill.
    runner.kill_now();

    if cfg!(windows) {
        assert_eq!(started["abrupt_cleanup"], "whole_tree");
        assert!(
            wait_until(
                || !pid_is_alive(root) && !pid_is_alive(grandchild),
                Duration::from_secs(15)
            ),
            "the root (PID {root}) or grandchild (PID {grandchild}) survived abrupt runner \
             death despite the reported whole_tree guarantee"
        );
    } else if cfg!(target_os = "linux") {
        assert_eq!(started["abrupt_cleanup"], "direct_child_only");
        assert!(
            wait_until(|| !pid_is_alive(root), Duration::from_secs(5)),
            "the direct child (PID {root}) survived abrupt runner death despite PDEATHSIG"
        );
        assert!(
            pid_is_alive(grandchild),
            "the grandchild (PID {grandchild}) unexpectedly died; the test must expose the \
             documented direct-child-only limitation"
        );
        assert!(
            wait_until(|| !pid_is_alive(grandchild), Duration::from_secs(15)),
            "the bounded grandchild (PID {grandchild}) did not self-terminate"
        );
    } else {
        assert_eq!(started["abrupt_cleanup"], "none");
        assert!(
            pid_is_alive(root) && pid_is_alive(grandchild),
            "the root (PID {root}) or grandchild (PID {grandchild}) unexpectedly died; the \
             test must expose the documented lack of abrupt cleanup"
        );
        assert!(
            wait_until(
                || !pid_is_alive(root) && !pid_is_alive(grandchild),
                Duration::from_secs(15)
            ),
            "the bounded root (PID {root}) or grandchild (PID {grandchild}) did not \
             self-terminate"
        );
    }
}

/// A descendant that keeps the inherited stdout handle open after the root exits
/// must not make the runner hang: teardown reaps the tree, which closes the pipe.
/// Proven with an upper bound — a runner that waited on pipe EOF would block until
/// the grandchild's own bounded sleep, far past the deadline here.
#[test]
fn a_descendant_holding_stdout_does_not_hang_the_runner() {
    let scenario = Scenario::new("e2e-holds-pipe");
    let pidfile = scenario.path("grandchild.pid");
    let pidfile_arg = pidfile.to_string_lossy().into_owned();

    let runner = command_with_flags(
        &scenario.dir,
        &[],
        &[],
        helper(
            "root",
            &[
                "--pidfile",
                &pidfile_arg,
                "--hold-stdout",
                "--grandchild-sleep-secs",
                "120",
            ],
        ),
    )
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn the runner");
    let mut runner = ChildGuard::new(runner);

    // Comfortably below the grandchild's 120s sleep: a genuine pipe-EOF hang could
    // only end when the grandchild self-terminates, so exceeding this is a hang.
    let bound = Duration::from_secs(30);
    let started = Instant::now();
    let status = wait_child_bounded(runner.child_mut(), bound);
    let elapsed = started.elapsed();

    let status = status.unwrap_or_else(|| {
        // `runner` (a ChildGuard) kills the hung process as this panic unwinds.
        panic!(
            "the runner did not return within {bound:?} while a descendant held the stdout \
             pipe — it must not wait on a leaked pipe handle"
        )
    });
    assert!(
        elapsed < bound,
        "the run completed but only at the deadline ({elapsed:?})"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "the root still exits cleanly even though a descendant held the pipe"
    );

    // Reaping the grandchild is what closed the pipe; confirm it is gone.
    if let Some(grandchild) = read_pid(&pidfile) {
        assert!(
            wait_until(|| !pid_is_alive(grandchild), Duration::from_secs(10)),
            "the pipe-holding grandchild (PID {grandchild}) survived teardown"
        );
    }
}

/// A rapid launch → exit → relaunch storm churns PID allocation around an
/// unrelated bystander. Each run must forward its child's clean exit, and the
/// bystander — never a member of any run's container — must survive untouched
/// (still alive and still heartbeating), proving no run kills or misattributes a
/// process outside its own tree even as PIDs recycle. Deterministically forcing a
/// PID *collision* is not possible, so this is a best-effort stress of the
/// property, not a proof of one specific reuse.
#[test]
fn rapid_run_churn_does_not_touch_an_unrelated_bystander() {
    let scenario = Scenario::new("e2e-pid-churn");
    let heartbeat = scenario.path("bystander.hb");
    let heartbeat_arg = heartbeat.to_string_lossy().into_owned();

    let bystander = Command::new(helper_bin())
        .args(["spin", "--sleep-secs", "120", "--heartbeat", &heartbeat_arg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the bystander");
    let mut bystander = ChildGuard::new(bystander);
    let bystander_pid = bystander.child_mut().id();

    assert!(
        wait_for_file_nonempty(&heartbeat, Duration::from_secs(20)),
        "the bystander never started heartbeating"
    );

    // The storm: many quick runs whose leaf exits at once, recycling PIDs.
    let iterations = 40;
    for i in 0..iterations {
        let jsonl = scenario.path(&format!("burst-{i}.jsonl"));
        let out = Command::new(bin())
            .arg("run")
            .arg("--jsonl")
            .arg(&jsonl)
            .arg("--")
            .arg(helper_bin())
            .args(["exit", "--code", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .expect("run a burst iteration");
        assert_eq!(
            out.status.code(),
            Some(0),
            "burst iteration {i} did not forward the leaf's clean exit"
        );
    }

    // The bystander — never a member of any run's container — must be untouched:
    // still alive and still heartbeating after the churn. `pid_is_alive` alone does
    // not prove "alive": on Unix the bystander is a direct child of the test process,
    // reaped only at the very end, so had a run's teardown collaterally killed it, it
    // would linger as a zombie and `kill(pid, 0)` would still report it as alive. The
    // real liveness proof is that the heartbeat file keeps *growing from here on out* —
    // a zombie or a suspended process cannot advance it. So take a fresh baseline now,
    // after the storm has settled, and require the file to grow past it; growth measured
    // against a pre-storm baseline would be trivially satisfied by bytes written during
    // the churn.
    assert!(
        pid_is_alive(bystander_pid),
        "the bystander (PID {bystander_pid}) was collaterally killed during run churn"
    );
    let heartbeat_after = file_len(&heartbeat);
    assert!(
        wait_until(
            || file_len(&heartbeat) > heartbeat_after,
            Duration::from_secs(10),
        ),
        "the bystander stopped heartbeating during run churn (suspended, killed, or a zombie?)"
    );

    bystander.kill_now();
}

/// How many record files (`*.json`) the registry directory holds right now.
fn registry_record_count(dir: &std::path::Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
            .count(),
        Err(_) => 0,
    }
}

/// Run `inspect --run-id <id> --json` against a scratch registry, and wait for it.
fn inspect(registry: &std::path::Path, run_id: &str) -> std::process::Output {
    Command::new(bin())
        .args(["inspect", "--run-id", run_id, "--json"])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .expect("spawn the inspect client")
}

/// The control plane end to end: `inspect` reaches a live containment run over the
/// local transport and reports the *real* container — its root PID and a non-empty
/// member list — and, once the runner dies abruptly (its `Drop`/cleanup skipped),
/// detects the run is gone with the reserved `CONTROL` code rather than hanging on a
/// dead endpoint. The helper root self-bounds, so nothing leaks if the test aborts.
#[test]
fn inspect_reads_a_live_run_and_detects_an_abrupt_death() {
    let scenario = Scenario::new("e2e-inspect");
    let registry = scenario.path("registry");
    let pidfile = scenario.path("grandchild.pid");
    let pidfile_arg = pidfile.to_string_lossy().into_owned();

    let runner = command_with_flags(
        &scenario.dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "e2e-run"],
        helper(
            "root",
            &[
                "--pidfile",
                &pidfile_arg,
                "--root-sleep-secs",
                "120",
                "--grandchild-sleep-secs",
                "120",
            ],
        ),
    )
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn the runner");
    let mut runner = ChildGuard::new(runner);

    // The run is inspectable once it has published its registry record + endpoint.
    assert!(
        wait_until(
            || registry_record_count(&registry) == 1,
            Duration::from_secs(20)
        ),
        "the run never registered, so it could not be inspected"
    );

    // Inspect the live run: it reports the run id, a mechanism, the root PID, and a
    // non-empty member list — the live container, reached over the local transport.
    let out = inspect(&registry, "e2e-run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "inspecting a live run succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snapshot: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("inspect prints a JSON snapshot line");
    assert_eq!(
        snapshot["run_id"], "e2e-run",
        "the snapshot names the run: {snapshot}"
    );
    assert!(
        snapshot["mechanism"].is_string(),
        "the snapshot names the containment mechanism: {snapshot}"
    );
    assert!(
        snapshot["root_pid"].as_u64().is_some(),
        "the snapshot carries the live root PID: {snapshot}"
    );
    assert!(
        snapshot["members"]
            .as_array()
            .is_some_and(|members| !members.is_empty()),
        "the snapshot lists the live container's members: {snapshot}"
    );

    // Abrupt death: kill the runner. Its `Drop`/cleanup never runs, so the entry is
    // left behind, but the OS releases its liveness lock. `inspect` must detect the
    // run is gone (a bounded CONTROL failure), never hang on the dead endpoint.
    runner.kill_now();
    assert!(
        wait_until(
            || inspect(&registry, "e2e-run").status.code() == Some(103),
            Duration::from_secs(15),
        ),
        "inspect must detect the dead runner as a CONTROL (103) failure, not hang"
    );
}

// ---------------------------------------------------------------------------
// Parallelism, cancellation, nested containment, and the MSBuild worker shape
// (T-011). These build on the same external-observation discipline as the tier
// above: liveness is read by PID from *outside* the runner, workers self-bound so
// nothing leaks on an abort, and processes the tier owns a handle for are torn down
// via `ChildGuard` (never by killing a bare, possibly-recycled PID).
// ---------------------------------------------------------------------------

/// The `event` type tag of a JSONL record, or `""` if absent — the shared accessor
/// the cross-check assertions below read the stream through.
fn event_tag(event: &serde_json::Value) -> &str {
    event["event"].as_str().unwrap_or_default()
}

/// The `root_pid` a run recorded in its `run_started` event at `jsonl`, if present.
/// Lets a scenario confirm two concurrent runs observed *distinct* roots.
fn run_started_root_pid(jsonl: &std::path::Path) -> Option<u32> {
    read_events(jsonl)
        .iter()
        .find(|event| event_tag(event) == "run_started")
        .and_then(|event| event["root_pid"].as_u64())
        .and_then(|pid| u32::try_from(pid).ok())
}

/// Run a mutating control client (`cancel` / `kill`) `--run-id <id>` against a
/// scratch registry and wait for it — the through-the-binary analogue of [`inspect`]
/// for the teardown verbs.
fn control_client(verb: &str, registry: &std::path::Path, run_id: &str) -> std::process::Output {
    Command::new(bin())
        .args([verb, "--run-id", run_id])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .expect("spawn the control client")
}

/// Whether a usable `dotnet` SDK is on this host — gates the real `dotnet build`
/// scenario, which otherwise skips **loudly** (see [`dotnet_build_leaves_no_reuse_worker`]).
fn dotnet_available() -> bool {
    Command::new("dotnet")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Two concurrent runs are contained independently: one run's teardown reaps only
/// its **own** tree and never touches the other's descendants. Run B stays alive
/// (its root sleeps) so its leaked grandchild is alive throughout; run A exits
/// cleanly, so A's container tears down and reaps A's grandchild while B is still
/// running. Afterwards A's grandchild is gone, B's is untouched, and each run's JSONL
/// names its own distinct root — independent observation per run. A clean exit (not
/// an abrupt kill) is used deliberately so the reap is whole-tree on every platform,
/// keeping the cross-run isolation claim free of the abrupt-death tri-state (K-005).
#[test]
fn concurrent_runs_do_not_touch_each_others_trees() {
    let scenario_a = Scenario::new("e2e-parallel-a");
    let scenario_b = Scenario::new("e2e-parallel-b");
    let registry_a = scenario_a.path("registry");
    let registry_b = scenario_b.path("registry");
    let pid_a = scenario_a.path("grandchild.pid");
    let pid_b = scenario_b.path("grandchild.pid");
    let pid_a_arg = pid_a.to_string_lossy().into_owned();
    let pid_b_arg = pid_b.to_string_lossy().into_owned();

    // Long-lived run B: its root stays alive, so its leaked grandchild must survive
    // A's whole lifecycle. It is the tree that must *not* be touched.
    let runner_b = command_with_flags(
        &scenario_b.dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry_b.as_path())],
        &["--run-id", "e2e-par-b"],
        helper(
            "root",
            &[
                "--pidfile",
                &pid_b_arg,
                "--root-sleep-secs",
                "120",
                "--grandchild-sleep-secs",
                "120",
            ],
        ),
    )
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn run B");
    let mut runner_b = ChildGuard::new(runner_b);

    assert!(
        wait_for_file_nonempty(&pid_b, Duration::from_secs(20)),
        "run B never recorded its grandchild PID"
    );
    let grandchild_b = read_pid(&pid_b).expect("run B recorded its grandchild PID");
    assert!(
        pid_is_alive(grandchild_b),
        "run B's grandchild (PID {grandchild_b}) should be alive before run A tears down"
    );

    // Run A to completion: its root exits at once, so A's container tears down and
    // reaps A's grandchild while B is still running.
    let out_a = run_with_flags(
        &scenario_a.dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry_a.as_path())],
        &["--run-id", "e2e-par-a"],
        helper(
            "root",
            &["--pidfile", &pid_a_arg, "--grandchild-sleep-secs", "120"],
        ),
    );
    assert_eq!(
        out_a.status.code(),
        Some(0),
        "run A's root exits cleanly; stderr: {}",
        String::from_utf8_lossy(&out_a.stderr)
    );

    let grandchild_a = read_pid(&pid_a).expect("run A recorded its grandchild PID");
    assert_ne!(
        grandchild_a, grandchild_b,
        "the two runs must leak distinct grandchildren"
    );

    // A's own grandchild is reaped by A's teardown…
    assert!(
        wait_until(|| !pid_is_alive(grandchild_a), Duration::from_secs(15)),
        "run A's own grandchild (PID {grandchild_a}) survived A's teardown"
    );
    // …and B's tree is left entirely alone: A's teardown never reached across runs.
    assert!(
        pid_is_alive(grandchild_b),
        "run A's teardown collaterally killed run B's grandchild (PID {grandchild_b}) — \
         containment is not per-run"
    );

    // Independent observation: each run's JSONL names its own, distinct root PID.
    let root_a = run_started_root_pid(&events_path(&scenario_a.dir))
        .expect("run A recorded a root PID in run_started");
    let root_b = run_started_root_pid(&events_path(&scenario_b.dir))
        .expect("run B recorded a root PID in run_started");
    assert_ne!(
        root_a, root_b,
        "each concurrent run must observe its own distinct root PID"
    );

    runner_b.kill_now();
}

/// A `Ctrl-C` mid-run is a **distinguishable** ending observed from outside the
/// reaped runner: the runner exits with the reserved `CANCELLED` code (107), its
/// JSONL records a `cancelled` event with `source` `ctrl_c` and a terminal
/// `runner_exit`, and the leaked grandchild is reaped (PID-observed). Unix-only: it
/// delivers a real `SIGINT` to the runner alone — an isolated Ctrl-C cannot be sent
/// to a single child on Windows, where the interrupt-style ending is instead covered
/// by the control-plane `cancel` scenario (which drives the same teardown).
#[cfg(unix)]
#[test]
fn ctrl_c_cancel_reports_the_cancel_code_and_reaps_the_tree() {
    let scenario = Scenario::new("e2e-ctrl-c");
    let pidfile = scenario.path("grandchild.pid");
    let pidfile_arg = pidfile.to_string_lossy().into_owned();

    let runner = command_with_flags(
        &scenario.dir,
        &[],
        &["--grace", "1s"],
        helper(
            "root",
            &[
                "--pidfile",
                &pidfile_arg,
                "--root-sleep-secs",
                "120",
                "--grandchild-sleep-secs",
                "120",
            ],
        ),
    )
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn the runner");
    let mut runner = ChildGuard::new(runner);
    let runner_pid = runner.child_mut().id();

    assert!(
        wait_for_file_nonempty(&pidfile, Duration::from_secs(20)),
        "the root never recorded a grandchild PID"
    );
    let grandchild = read_pid(&pidfile).expect("the root recorded the grandchild PID");
    assert!(
        pid_is_alive(grandchild),
        "the grandchild (PID {grandchild}) should be alive before the Ctrl-C"
    );

    // Deliver the interactive Ctrl-C (SIGINT) the runner listens for — to the runner
    // alone (its PID), not a process group, so only the runner sees it.
    let rc = unsafe { libc::kill(runner_pid as libc::pid_t, libc::SIGINT) };
    assert_eq!(rc, 0, "failed to deliver SIGINT to the runner");

    let status = wait_child_bounded(runner.child_mut(), Duration::from_secs(20))
        .expect("the runner exited after the Ctrl-C");
    assert_eq!(
        status.code(),
        Some(107),
        "a Ctrl-C cancel must exit with the reserved CANCELLED code"
    );

    // The tree is reaped, observed from outside the runner.
    assert!(
        wait_until(|| !pid_is_alive(grandchild), Duration::from_secs(15)),
        "the leaked grandchild (PID {grandchild}) survived the Ctrl-C teardown"
    );

    // The stream records the cancel as a distinguishable `ctrl_c` ending.
    let events = read_events(&events_path(&scenario.dir));
    assert!(
        events
            .iter()
            .any(|event| event_tag(event) == "cancelled" && event["source"] == "ctrl_c"),
        "the JSONL must record a cancelled/ctrl_c event"
    );
    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(
        event_tag(runner_exit),
        "runner_exit",
        "runner_exit is terminal"
    );
    assert_eq!(runner_exit["source"], "cancelled");
    assert_eq!(runner_exit["code"], 107);
}

/// The shared body of the control-plane `cancel` / `kill` scenarios: a `verb`
/// targeting a run by `run_id` reaps **only** that run's container and leaves a
/// separately-spawned bystander untouched. The runner ends with `expected_code`, and
/// its JSONL records the ending as externally initiated (`ending_event` /
/// `ending_source`), so an observer reading `--jsonl` — not just the control client —
/// sees the outside command. The teardown is the run's own soft-stop→grace→hard-kill
/// (cancel) or immediate hard kill (kill), which reaps the whole tree on every
/// platform, so the isolation claim needs no abrupt-death assumption (K-005).
fn control_verb_reaps_only_the_target(
    scenario_tag: &str,
    run_id: &str,
    verb: &str,
    expected_code: i32,
    ending_event: &str,
    ending_source: &str,
) {
    let scenario = Scenario::new(scenario_tag);
    let registry = scenario.path("registry");
    let pidfile = scenario.path("grandchild.pid");
    let pidfile_arg = pidfile.to_string_lossy().into_owned();
    let heartbeat = scenario.path("bystander.hb");
    let heartbeat_arg = heartbeat.to_string_lossy().into_owned();

    // A standalone bystander, never handed to the run: it must be untouched. It
    // heartbeats so we can confirm it is still *running* (not merely still a PID)
    // after the target is torn down.
    let bystander = Command::new(helper_bin())
        .args(["spin", "--sleep-secs", "120", "--heartbeat", &heartbeat_arg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the bystander");
    let mut bystander = ChildGuard::new(bystander);
    let bystander_pid = bystander.child_mut().id();
    assert!(
        wait_for_file_nonempty(&heartbeat, Duration::from_secs(20)),
        "the bystander never started heartbeating"
    );

    // The target run: a long-lived root leaking a long-lived grandchild, registered
    // under `run_id` in the scratch registry the control client will consult.
    let runner = command_with_flags(
        &scenario.dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", run_id],
        helper(
            "root",
            &[
                "--pidfile",
                &pidfile_arg,
                "--root-sleep-secs",
                "120",
                "--grandchild-sleep-secs",
                "120",
            ],
        ),
    )
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn the target runner");
    let mut runner = ChildGuard::new(runner);

    // Only act once the run is actually reachable over the control plane (its
    // registry record + endpoint published and its server accepting), so the verb
    // does not race an unready runner.
    assert!(
        wait_until(
            || inspect(&registry, run_id).status.code() == Some(0),
            Duration::from_secs(20)
        ),
        "the run never became reachable over the control plane"
    );
    // The run registers its endpoint *before* the child spawns, so reachability can
    // race ahead of the leaked grandchild: wait for the root to record its PID too.
    assert!(
        wait_for_file_nonempty(&pidfile, Duration::from_secs(20)),
        "the root never recorded a grandchild PID"
    );
    let grandchild = read_pid(&pidfile).expect("the root recorded the grandchild PID");
    assert!(
        pid_is_alive(grandchild) && pid_is_alive(bystander_pid),
        "the target's grandchild (PID {grandchild}) and the bystander (PID {bystander_pid}) \
         should both be alive before the {verb}"
    );

    // Act on the target by id alone.
    let out = control_client(verb, &registry, run_id);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the {verb} client should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ack: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the control client prints a JSON ack line");
    assert_eq!(
        ack["accepted"], true,
        "the runner accepted the {verb}: {ack}"
    );
    assert_eq!(ack["action"], verb, "the ack echoes the verb: {ack}");
    assert_eq!(ack["run_id"], run_id, "the ack names the target run: {ack}");

    // The runner ends with the verb's reserved control code.
    let status = wait_child_bounded(runner.child_mut(), Duration::from_secs(20))
        .expect("the runner exited after the control command");
    assert_eq!(
        status.code(),
        Some(expected_code),
        "the target run must exit with the reserved control code {expected_code}"
    );

    // The whole target tree is reaped…
    assert!(
        wait_until(|| !pid_is_alive(grandchild), Duration::from_secs(15)),
        "the target run's grandchild (PID {grandchild}) survived the {verb} teardown"
    );
    // …but the standalone bystander is untouched: still alive and still heartbeating.
    // `pid_is_alive` alone does not prove "alive": on Unix the bystander is a direct
    // child of the test process, reaped only at the very end, so had the {verb}'s
    // teardown collaterally killed it, it would linger as a zombie and `kill(pid, 0)`
    // would still report it as alive. The real liveness proof is that the heartbeat
    // file keeps *growing from here on out* — a zombie or a suspended process cannot
    // advance it. So take a fresh baseline now, after the teardown has settled, and
    // require the file to grow past it; growth measured against a pre-teardown baseline
    // would be trivially satisfied by bytes written before the {verb} ever ran.
    assert!(
        pid_is_alive(bystander_pid),
        "the bystander (PID {bystander_pid}) was collaterally reaped by the {verb}"
    );
    let heartbeat_after = file_len(&heartbeat);
    assert!(
        wait_until(
            || file_len(&heartbeat) > heartbeat_after,
            Duration::from_secs(10)
        ),
        "the bystander stopped heartbeating after the {verb} (suspended, killed, or a zombie?)"
    );

    // The stream records an externally-initiated ending an outside observer can read.
    let events = read_events(&events_path(&scenario.dir));
    assert!(
        events
            .iter()
            .any(|event| event_tag(event) == ending_event && event["source"] == ending_source),
        "the JSONL must record a {ending_event}/{ending_source} ending"
    );
    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(
        event_tag(runner_exit),
        "runner_exit",
        "runner_exit is terminal"
    );
    assert_eq!(runner_exit["source"], ending_source);
    assert_eq!(runner_exit["code"], expected_code);
    assert!(
        runner_exit["child_code"].is_null(),
        "a control-initiated ending forwards no child code: {runner_exit}"
    );

    bystander.kill_now();
}

/// `cancel --run-id <id>` over the control plane runs the shared soft-stop → grace →
/// hard-kill teardown of **only** the target run (exit 108, a `cancelled` /
/// `control_cancel` ending) and never touches a separately-spawned bystander.
#[test]
fn control_plane_cancel_reaps_only_the_target_run() {
    control_verb_reaps_only_the_target(
        "e2e-control-cancel",
        "e2e-cancel",
        "cancel",
        108,
        "cancelled",
        "control_cancel",
    );
}

/// `kill --run-id <id>` over the control plane hard-kills **only** the target run's
/// tree immediately (exit 109, a `killed` / `control_kill` ending) and leaves a
/// separately-spawned bystander alive.
#[test]
fn control_plane_kill_reaps_only_the_target_run() {
    control_verb_reaps_only_the_target(
        "e2e-control-kill",
        "e2e-kill",
        "kill",
        109,
        "killed",
        "control_kill",
    );
}

/// Windows-only: a `run` launched from an environment that is **already inside a Job
/// Object** (a terminal, build server, or IDE) still stands up and tears down its own
/// *nested* container. The `job-parent` helper self-places into an outer, plain Job
/// Object and then spawns `run` as its child; `run` must create its inner Job Object
/// (nested inside the outer one) without an attach/create error, and its kill-on-drop
/// must still reap the leaked grandchild. Observed from outside: the grandchild PID is
/// gone once the wrapper returns, and the run's JSONL shows the `job_object` mechanism
/// and a completed teardown.
#[cfg(windows)]
#[test]
fn run_nested_in_a_job_object_still_contains_its_tree() {
    let scenario = Scenario::new("e2e-nested-job");
    let pidfile = scenario.path("grandchild.pid");
    let pidfile_arg = pidfile.to_string_lossy().into_owned();
    let jsonl = events_path(&scenario.dir);
    let jsonl_arg = jsonl.to_string_lossy().into_owned();

    // Build the inner `run` invocation, then wrap it in `job-parent -- <run …>` so it
    // is launched from inside the wrapper's outer Job Object.
    let run_command = vec![
        bin().to_string(),
        "run".to_string(),
        "--jsonl".to_string(),
        jsonl_arg,
        "--run-id".to_string(),
        "e2e-nested".to_string(),
        "--".to_string(),
        helper_bin().to_string(),
        "root".to_string(),
        "--pidfile".to_string(),
        pidfile_arg,
        "--grandchild-sleep-secs".to_string(),
        "120".to_string(),
    ];
    let mut wrapper_args = vec!["job-parent".to_string(), "--".to_string()];
    wrapper_args.extend(run_command);

    let wrapper = Command::new(helper_bin())
        .args(&wrapper_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the job-parent wrapper");
    let mut wrapper = ChildGuard::new(wrapper);

    // The wrapper self-places into the outer job, runs `run` (which reaps its tree and
    // exits 0), and forwards that 0.
    let status = wait_child_bounded(wrapper.child_mut(), Duration::from_secs(60))
        .expect("the job-parent wrapper exited");
    assert_eq!(
        status.code(),
        Some(0),
        "a run nested inside an outer Job Object must complete cleanly (no nested \
         attach/create error)"
    );

    // Containment held despite the nesting: the leaked grandchild is reaped.
    let grandchild = read_pid(&pidfile).expect("the root recorded the grandchild PID");
    assert!(
        wait_until(|| !pid_is_alive(grandchild), Duration::from_secs(15)),
        "the nested container failed to reap the leaked grandchild (PID {grandchild})"
    );

    // The stream confirms the Job Object mechanism and a completed teardown.
    let events = read_events(&jsonl);
    let started = events
        .iter()
        .find(|event| event_tag(event) == "run_started")
        .expect("run_started");
    assert_eq!(
        started["mechanism"], "job_object",
        "the nested run still uses the Job Object mechanism: {started}"
    );
    assert!(
        events
            .iter()
            .any(|event| event_tag(event) == "cleanup_finished"),
        "the nested run must record teardown via cleanup_finished"
    );
    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(
        event_tag(runner_exit),
        "runner_exit",
        "runner_exit is terminal"
    );
    assert_eq!(
        runner_exit["child_code"], 0,
        "the nested run forwards the child's clean code: {runner_exit}"
    );
}

/// A real Windows console survives both process boundaries: the dedicated host
/// allocates fresh `CONIN$`/`CONOUT$` handles, the runner inherits them, and
/// `--inherit-stdio` passes them through ProcessKit to a terminal-aware child.
/// The result is written out-of-band so the assertion itself never redirects the
/// handles being tested.
#[cfg(windows)]
#[test]
fn inherited_stdio_preserves_real_windows_console_handles() {
    let scenario = Scenario::new("e2e-inherit-stdio-console");
    let jsonl = events_path(&scenario.dir);
    let report = scenario.path("stdio-report.txt");
    let jsonl_arg = jsonl.to_string_lossy().into_owned();
    let report_arg = report.to_string_lossy().into_owned();

    let host = Command::new(helper_bin())
        .args([
            "console-parent",
            "--runner",
            bin(),
            "--jsonl",
            &jsonl_arg,
            "--report",
            &report_arg,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the dedicated Windows console host");
    let mut host = ChildGuard::new(host);
    let status = wait_child_bounded(host.child_mut(), Duration::from_secs(30))
        .expect("the console-hosted runner exited");
    assert_eq!(
        status.code(),
        Some(0),
        "the console-hosted inherited-stdio run must complete cleanly"
    );

    let observed = std::fs::read_to_string(&report).expect("read terminal-status report");
    assert_eq!(
        observed, "stdin=true\nstdout=true\nstderr=true\ninput=\n",
        "the contained child must see all three inherited handles as console terminals"
    );

    let events = read_events(&jsonl);
    let cleanup = events
        .iter()
        .find(|event| event_tag(event) == "cleanup_finished")
        .expect("the interactive run records completed containment cleanup");
    assert_eq!(cleanup["remaining"], 0);
    assert_eq!(cleanup["remaining_pids"], serde_json::json!([]));
    let runner_exit = events.last().expect("a terminal lifecycle event");
    assert_eq!(event_tag(runner_exit), "runner_exit");
    assert_eq!(runner_exit["source"], "child_exit");
    assert_eq!(runner_exit["child_code"], 0);
}

/// A Unix pseudo-terminal proves more than descriptor inheritance: the contained
/// child sees all streams as terminals and successfully reads. On targets where
/// ProcessKit uses a separate process group, the runner temporarily hands that
/// group foreground control and restores its own group after cleanup.
#[cfg(unix)]
#[test]
fn inherited_stdio_preserves_a_usable_posix_terminal() {
    let scenario = Scenario::new("e2e-inherit-stdio-pty");
    let jsonl = events_path(&scenario.dir);
    let report = scenario.path("stdio-report.txt");
    let jsonl_arg = jsonl.to_string_lossy().into_owned();
    let report_arg = report.to_string_lossy().into_owned();

    let host = Command::new(helper_bin())
        .args([
            "pty-parent",
            "--runner",
            bin(),
            "--jsonl",
            &jsonl_arg,
            "--report",
            &report_arg,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the dedicated Unix pty host");
    let mut host = ChildGuard::new(host);
    let status = wait_child_bounded(host.child_mut(), Duration::from_secs(30)).unwrap_or_else(|| {
        let report_state = std::fs::read_to_string(&report)
            .map_or_else(|err| format!("unavailable: {err}"), |text| format!("{text:?}"));
        let event_state = std::fs::read_to_string(&jsonl)
            .map_or_else(|err| format!("unavailable: {err}"), |text| format!("{text:?}"));
        panic!(
            "the pty-hosted runner did not exit; child report={report_state}; JSONL={event_state}"
        )
    });
    assert_eq!(
        status.code(),
        Some(0),
        "the pty-hosted inherited-stdio run must complete cleanly"
    );

    let observed = std::fs::read_to_string(&report).expect("read terminal-status report");
    assert_eq!(
        observed, "stdin=true\nstdout=true\nstderr=true\ninput=pty line\n",
        "the child must see a usable terminal and read the supplied pty input"
    );

    let events = read_events(&jsonl);
    let cleanup = events
        .iter()
        .find(|event| event_tag(event) == "cleanup_finished")
        .expect("the interactive run records completed containment cleanup");
    assert_eq!(cleanup["remaining"], 0);
    assert_eq!(cleanup["remaining_pids"], serde_json::json!([]));
    let runner_exit = events.last().expect("a terminal lifecycle event");
    assert_eq!(event_tag(runner_exit), "runner_exit");
    assert_eq!(runner_exit["source"], "child_exit");
    assert_eq!(runner_exit["child_code"], 0);
}

/// The project's raison d'être, cross-platform and deterministic: a leaked,
/// long-lived worker of the **MSBuild node-reuse shape** does not survive `run`. The
/// runner's program argv carries the reusable-node markers (`MSBuild.dll`,
/// `/nodemode:1`, `/nodeReuse:true`), so the binary must also classify the run with
/// the `msbuild_node_reuse` hint end-to-end; the leaked grandchild stands in for the
/// persistent worker node (`dotnet`-free, so it runs on all three OSes). After `run`
/// returns, the worker PID is gone (observed from outside) and the stream shows the
/// hint, a completed teardown, and the child's clean code. The real `dotnet build`
/// path is [`dotnet_build_leaves_no_reuse_worker`].
#[test]
fn msbuild_node_reuse_worker_is_reaped() {
    let scenario = Scenario::new("e2e-msbuild-synth");
    let pidfile = scenario.path("worker.pid");
    let pidfile_arg = pidfile.to_string_lossy().into_owned();

    let out = run_with_flags(
        &scenario.dir,
        &[],
        &[],
        helper(
            "root",
            &[
                "--pidfile",
                &pidfile_arg,
                "--grandchild-sleep-secs",
                "120",
                // The reusable-node markers: ignored by the helper, but part of the
                // runner's program argv, so the hint classifier recognizes the shape.
                "MSBuild.dll",
                "/nodemode:1",
                "/nodeReuse:true",
            ],
        ),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "the MSBuild-host root exits cleanly; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The leaked worker node is reaped by the container teardown, observed by PID.
    let worker = read_pid(&pidfile).expect("the host recorded the worker-node PID");
    assert!(
        wait_until(|| !pid_is_alive(worker), Duration::from_secs(15)),
        "a leaked MSBuild-shaped worker node (PID {worker}) survived teardown — the \
         project's core guarantee"
    );

    // Through the binary: the run is classified as an MSBuild node-reuse shape.
    let events = read_events(&events_path(&scenario.dir));
    let started = events
        .iter()
        .find(|event| event_tag(event) == "run_started")
        .expect("run_started");
    assert_eq!(
        started["command"]["hint"], "msbuild_node_reuse",
        "the runner classifies the MSBuild node-reuse shape: {started}"
    );
    assert!(
        events
            .iter()
            .any(|event| event_tag(event) == "cleanup_finished"),
        "the run must record teardown via cleanup_finished"
    );
    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(
        event_tag(runner_exit),
        "runner_exit",
        "runner_exit is terminal"
    );
    assert_eq!(
        runner_exit["child_code"], 0,
        "the MSBuild-host run forwards the child's clean code: {runner_exit}"
    );
}

/// The real `dotnet build` shape, gated on a usable SDK. When `dotnet` is present a
/// trivial project is built **through** `run` with node reuse on (`-nodeReuse:true`),
/// so MSBuild spawns the persistent `MSBuild.dll /nodemode:1 /nodeReuse:true` worker
/// nodes this project exists to contain; `run` must contain the build end-to-end and
/// tear the whole tree down (no worker node — nor its console host — survives). When
/// `dotnet` is **absent** the scenario skips **loudly and visibly** (a `SKIP` line
/// under `--nocapture`), never silently — the definitive cross-platform proof of the
/// same shape is [`msbuild_node_reuse_worker_is_reaped`].
#[test]
fn dotnet_build_leaves_no_reuse_worker() {
    if !dotnet_available() {
        println!(
            "SKIP dotnet_build_leaves_no_reuse_worker: `dotnet` is not installed on this host. \
             The MSBuild node-reuse worker shape is proven cross-platform and deterministically \
             by `msbuild_node_reuse_worker_is_reaped` (a synthetic worker of the same shape)."
        );
        return;
    }

    let scenario = Scenario::new("e2e-dotnet");
    let project_dir = scenario.path("proj");
    std::fs::create_dir_all(&project_dir).expect("create the project directory");
    // A minimal SDK-style project with no NuGet package references, so the implicit
    // restore is satisfied offline from the SDK's bundled packs.
    std::fs::write(
        project_dir.join("proj.csproj"),
        "<Project Sdk=\"Microsoft.NET.Sdk\">\n  \
         <PropertyGroup>\n    \
         <OutputType>Exe</OutputType>\n    \
         <TargetFramework>net8.0</TargetFramework>\n  \
         </PropertyGroup>\n</Project>\n",
    )
    .expect("write proj.csproj");
    std::fs::write(
        project_dir.join("Program.cs"),
        "internal static class Program { private static void Main() { } }\n",
    )
    .expect("write Program.cs");
    let csproj = project_dir
        .join("proj.csproj")
        .to_string_lossy()
        .into_owned();

    // Build through the runner with node reuse explicitly on (`-m` enables the
    // multi-node build that spins up worker nodes). The point is containment and
    // teardown, not the build result, so the exit code is not asserted.
    let start = Instant::now();
    let out = run_with_flags(
        &scenario.dir,
        &[],
        &[],
        vec![
            "dotnet".to_string(),
            "build".to_string(),
            csproj,
            "-nodeReuse:true".to_string(),
            "-m".to_string(),
        ],
    );
    let elapsed = start.elapsed();

    // No hang: `run` returned rather than waiting on a leaked, long-lived reuse node.
    assert!(
        elapsed < Duration::from_secs(300),
        "the contained `dotnet build` did not return in time ({elapsed:?}) — a leaked \
         worker node may have hung the runner"
    );

    // The runner contained the build end-to-end and tore it down.
    let events = read_events(&events_path(&scenario.dir));
    assert!(
        events.iter().any(|event| event_tag(event) == "run_started"),
        "the contained dotnet build must emit run_started"
    );
    let cleanup = events
        .iter()
        .rev()
        .find(|event| event_tag(event) == "cleanup_finished")
        .expect("the contained dotnet build must record teardown via cleanup_finished");
    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(
        event_tag(runner_exit),
        "runner_exit",
        "runner_exit is terminal"
    );

    // On Windows the Job Object teardown is atomic, so the post-kill member set is
    // empty — no MSBuild worker node (nor its console host) can outlive the run.
    if cfg!(windows) {
        assert_eq!(
            cleanup["remaining"], 0,
            "a Job Object teardown must leave no MSBuild worker node behind: {cleanup}"
        );
    }

    println!(
        "dotnet_build_leaves_no_reuse_worker: contained a real `dotnet build` (exit {:?}) and \
         tore its tree down in {elapsed:?}",
        out.status.code()
    );
}

// ---------------------------------------------------------------------------
// `--env` / `--env-remove` / `--env-clear` (T-165): the child echoes one or more
// named environment variables through a tiny platform shell one-liner, proving
// each flag's effect through the built binary — the value this tier adds over a
// unit test of the CLI parser (`src/cli.rs`) or of `run.rs`'s builder calls.
// ---------------------------------------------------------------------------

/// The platform one-liner that echoes each named env var on its own line. `cmd
/// /c echo %VAR%` prints the *literal* text `%VAR%` when the variable is unset (a
/// well-known cmd quirk); `sh -c 'echo "$VAR"'` prints an empty line instead.
/// Either way, the assertions below key off whether a specific, distinctive value
/// string appears in stdout — never off how an *unset* variable renders — so
/// that platform difference never matters.
fn echo_env_script(names: &[&str]) -> String {
    if cfg!(windows) {
        names
            .iter()
            .map(|name| format!("echo %{name}%"))
            .collect::<Vec<_>>()
            .join("&")
    } else {
        names
            .iter()
            .map(|name| format!("echo \"${name}\""))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// `--env KEY=VALUE` sets a variable for the child that was never present in the
/// runner's own environment.
#[test]
fn env_sets_a_new_variable_for_the_child() {
    let scenario = Scenario::new("e2e-env-set");
    let out = run_with_flags(
        &scenario.dir,
        &[],
        &["--env", "PK_CLI_ENV_SET=set-by-env-flag"],
        shell_inline(&echo_env_script(&["PK_CLI_ENV_SET"])),
    );
    assert!(
        out.status.success(),
        "the child must exit cleanly; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("set-by-env-flag"),
        "--env must set the variable for the child: {stdout:?}"
    );
}

/// `--env-remove KEY` removes one variable the child would otherwise inherit from
/// the runner's own environment. Proved against a baseline run (no
/// `--env-remove`) that shows the variable really is inherited by default —
/// otherwise a "the value is absent" assertion alone would not distinguish
/// "removed" from "was never there".
#[test]
fn env_remove_removes_an_inherited_variable() {
    let runner_envs: &[(&str, &Path)] = &[(
        "PK_CLI_ENV_REMOVE_TARGET",
        Path::new("inherited-and-removed"),
    )];
    let script = || shell_inline(&echo_env_script(&["PK_CLI_ENV_REMOVE_TARGET"]));

    let baseline_scenario = Scenario::new("e2e-env-remove-baseline");
    let baseline = run_with_flags(&baseline_scenario.dir, runner_envs, &[], script());
    assert!(
        baseline.status.success(),
        "baseline child must exit cleanly; stderr: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    assert!(
        String::from_utf8_lossy(&baseline.stdout).contains("inherited-and-removed"),
        "baseline: the runner's own environment must reach the child by default \
         (without --env-remove) for this to be a meaningful proof"
    );

    let removed_scenario = Scenario::new("e2e-env-remove-removed");
    let removed = run_with_flags(
        &removed_scenario.dir,
        runner_envs,
        &["--env-remove", "PK_CLI_ENV_REMOVE_TARGET"],
        script(),
    );
    assert!(
        removed.status.success(),
        "removed-case child must exit cleanly; stderr: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&removed.stdout).contains("inherited-and-removed"),
        "--env-remove must strip the inherited variable from the child's environment"
    );
}

/// `--env-clear` wipes the child's entire inherited environment; an `--env` given
/// on the same run still sets its own explicit variable on top of that cleared
/// slate — the documented "clear, then remove, then set" applied order
/// (README.md, "Environment").
#[test]
fn env_clear_wipes_inherited_env_except_explicit_env() {
    let scenario = Scenario::new("e2e-env-clear");
    let runner_envs: &[(&str, &Path)] = &[(
        "PK_CLI_ENV_CLEAR_TARGET",
        Path::new("inherited-and-cleared"),
    )];
    let script = shell_inline(&echo_env_script(&[
        "PK_CLI_ENV_CLEAR_TARGET",
        "PK_CLI_ENV_CLEAR_KEEP",
    ]));

    let out = run_with_flags(
        &scenario.dir,
        runner_envs,
        &[
            "--env-clear",
            "--env",
            "PK_CLI_ENV_CLEAR_KEEP=kept-by-env-flag",
        ],
        script,
    );
    assert!(
        out.status.success(),
        "the child must exit cleanly; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("inherited-and-cleared"),
        "--env-clear must wipe the inherited variable: {stdout:?}"
    );
    assert!(
        stdout.contains("kept-by-env-flag"),
        "an explicit --env still sets its variable on top of the cleared slate: {stdout:?}"
    );
}
