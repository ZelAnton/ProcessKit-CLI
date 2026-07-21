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

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{
    ChildGuard, Scenario, bin, command_with_flags, events_path, file_len, helper_bin, pid_is_alive,
    read_events, read_pid, run_with_flags, wait_child_bounded, wait_for_file_nonempty, wait_until,
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

/// After an **abrupt** runner death (no destructors, so no kill-on-drop), the
/// kernel container must still reap the tree. That guarantee is the Windows Job
/// Object's kill-on-close; on Unix the equivalent hardening is opt-in and
/// direct-child-only (`processkit`'s `Command::kill_on_parent_death`), which this
/// runner does not use — so there is no whole-tree guarantee to assert, and the
/// scenario skips loudly rather than pretend one exists.
#[test]
fn abrupt_runner_death_still_reaps_the_tree() {
    if !cfg!(windows) {
        eprintln!(
            "SKIP abrupt_runner_death_still_reaps_the_tree: whole-tree reaping after an \
             abrupt runner kill is the Windows Job Object kill-on-close guarantee; on {} it \
             is not kernel-enforced for a leaked grandchild (that would need opt-in, \
             direct-child-only kill_on_parent_death), so there is nothing to assert here.",
            std::env::consts::OS
        );
        return;
    }

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

    assert!(
        wait_for_file_nonempty(&pidfile, Duration::from_secs(20)),
        "the root never recorded a grandchild PID"
    );
    let grandchild = read_pid(&pidfile).expect("the root recorded the grandchild PID");
    assert!(
        pid_is_alive(grandchild),
        "the grandchild (PID {grandchild}) should be alive before the runner is killed"
    );

    // Abrupt kill: `TerminateProcess` of the runner — no clean shutdown, no `Drop`.
    // Its sole Job Object handle closes with it, so the kernel reaps the tree.
    runner.kill_now();

    assert!(
        wait_until(|| !pid_is_alive(grandchild), Duration::from_secs(15)),
        "the leaked grandchild (PID {grandchild}) survived an abrupt runner death — the \
         Job Object kill-on-close did not reap the tree"
    );
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
    let heartbeat_before = file_len(&heartbeat);

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

    assert!(
        pid_is_alive(bystander_pid),
        "the bystander (PID {bystander_pid}) was collaterally killed during run churn"
    );
    assert!(
        wait_until(
            || file_len(&heartbeat) > heartbeat_before,
            Duration::from_secs(10),
        ),
        "the bystander stopped heartbeating during run churn (suspended or killed?)"
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
