//! Through-the-binary tests for `attest` — the containment-membership question a
//! caller can only ask about itself.
//!
//! Every scenario here is a **real cross-process connection**: the identity the
//! runner answers about is the one the operating system reports for the client that
//! actually opened the control socket / named pipe, so nothing in this file simulates
//! a peer. That is the property under test — a mocked identity would test the
//! bookkeeping around the fact rather than the fact itself.
//!
//! The shape used throughout is that an *in-run* client is produced by making the
//! run's own child be `processkit-cli attest`: a process that is a container member
//! because the runner put it there, not because anything said so. Its stdout is
//! echoed by the runner and its exit code is forwarded verbatim (`docs/exit-codes.md`,
//! "The core rule: child fidelity"), so one invocation yields both halves of the
//! answer.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{bin, headless_run_command, scratch, shell_inline};
use processkit_cli::registry::test_support::write_stale_entry;
use serde_json::Value;

/// `attest`'s own reserved code: the caller is definitely not a member.
const NOT_A_MEMBER: i32 = 115;
/// The shared "no answer you can act on" code every control client reports.
const CONTROL: i32 = 103;
/// A command line the parser refuses outright.
const USAGE: i32 = 100;

/// The registry every scenario points its commands at, isolated from the developer's
/// own runs and from the other test binaries'.
fn registry_dir(dir: &Path) -> PathBuf {
    dir.join("registry")
}

/// A `run` invocation against `registry`, writing its own event stream so several
/// runs can share one scratch directory. The `--timeout` is a backstop: a scenario
/// that panics before tearing its run down cannot leave a runner behind.
fn runner(dir: &Path, registry: &Path, run_id: &str, tag: &str) -> Command {
    let mut cmd = headless_run_command();
    cmd.arg("--jsonl")
        .arg(dir.join(format!("{tag}.jsonl")))
        .arg("--run-id")
        .arg(run_id)
        .arg("--timeout")
        .arg("60s")
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry);
    cmd
}

/// Run a child that attests, *inside* a run of its own, and wait for the whole thing.
/// `target` is the run the child asks about — usually, but deliberately not always,
/// the run containing it.
fn run_attesting(dir: &Path, registry: &Path, run_id: &str, tag: &str, target: &str) -> Output {
    runner(dir, registry, run_id, tag)
        .arg("--")
        .arg(bin())
        .args(["attest", "--run-id", target, "--json"])
        .output()
        .unwrap_or_else(|err| panic!("spawn the run whose child attests `{target}`: {err}"))
}

/// Ask from *this* test process — a caller that is inside no run at all.
fn attest_outside(registry: &Path, target: &str) -> Output {
    Command::new(bin())
        .args(["attest", "--run-id", target, "--json"])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .unwrap_or_else(|err| panic!("spawn an outside attest client for `{target}`: {err}"))
}

/// Start a long-lived run and return once its registry record advertises a control
/// endpoint — the point from which a client can actually reach it.
fn spawn_live_run(dir: &Path, registry: &Path, run_id: &str, tag: &str) -> Child {
    let child = runner(dir, registry, run_id, tag)
        .arg("--")
        .args(long_child())
        .spawn()
        .unwrap_or_else(|err| panic!("spawn the live run `{run_id}`: {err}"));
    wait_until(
        || published_endpoints(registry, run_id) >= 1,
        Duration::from_secs(20),
        &format!("the published record for `{run_id}`"),
    );
    child
}

fn long_child() -> Vec<String> {
    if cfg!(windows) {
        shell_inline("ping -n 300 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 300")
    }
}

/// How many registry records name `run_id` *and* already advertise an endpoint.
fn published_endpoints(registry: &Path, run_id: &str) -> usize {
    let Ok(read_dir) = std::fs::read_dir(registry) else {
        return 0;
    };
    read_dir
        .filter_map(Result::ok)
        .filter(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                return false;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                return false;
            };
            let Ok(record) = serde_json::from_str::<Value>(&text) else {
                return false;
            };
            record["run_id"] == Value::String(run_id.to_string()) && record["endpoint"].is_string()
        })
        .count()
}

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration, what: &str) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < timeout,
            "{what} did not happen in {timeout:?}"
        );
        sleep(Duration::from_millis(50));
    }
}

/// End a live run through the control plane and reap the runner process.
fn cancel_run(registry: &Path, run_id: &str, mut child: Child) {
    let out = Command::new(bin())
        .args(["kill", "--run-id", run_id])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .expect("spawn the kill client");
    assert!(
        out.status.success(),
        "killing `{run_id}` succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = child.wait();
}

/// The attestation an invocation printed, with its exit code asserted first — the two
/// halves of the answer, checked together so neither can pass on its own.
fn attestation(out: &Output, code: i32, what: &str) -> Value {
    assert_eq!(
        out.status.code(),
        Some(code),
        "{what} must exit {code}; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find(|line| line.contains("attestation_version"))
        .unwrap_or_else(|| panic!("{what} prints an attestation on stdout: {stdout}"));
    serde_json::from_str(line).unwrap_or_else(|err| panic!("{what}: {line}: {err}"))
}

/// Scenario 1 — **an in-run client succeeds.** The run's own child asks about the run
/// containing it, and the runner recognises it from the transport alone.
#[test]
fn a_client_inside_the_run_is_attested_as_a_member() {
    let dir = scratch("attest-inside");
    let registry = registry_dir(&dir);

    let out = run_attesting(&dir, &registry, "inside-42", "inside", "inside-42");
    let value = attestation(&out, 0, "an in-run attest");

    assert_eq!(value["verdict"], "member");
    assert_eq!(value["run_id"], "inside-42");
    assert_eq!(value["attestation_version"], 1);
    let peer_pid = value["peer_pid"]
        .as_u64()
        .expect("a decided verdict always names the peer the kernel reported");
    assert!(peer_pid > 0, "a real pid, not a placeholder: {value}");
    assert!(
        value["mechanism"].as_str().is_some_and(|m| !m.is_empty()),
        "the verdict names the containment it is about: {value}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Scenario 2 — **an outside client fails.** The same command, the same live run, a
/// caller that is simply not in it: a decided negative with its own exit code, not a
/// failure to reach anything.
#[test]
fn a_client_outside_every_run_is_not_a_member() {
    let dir = scratch("attest-outside");
    let registry = registry_dir(&dir);
    let child = spawn_live_run(&dir, &registry, "outside-42", "outside");

    let out = attest_outside(&registry, "outside-42");
    let value = attestation(&out, NOT_A_MEMBER, "an outside attest against a live run");
    assert_eq!(value["verdict"], "not_a_member");
    assert_eq!(value["run_id"], "outside-42");
    assert!(
        value["peer_pid"].as_u64().is_some_and(|pid| pid > 0),
        "the runner named the caller before deciding it is not a member: {value}"
    );

    cancel_run(&registry, "outside-42", child);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Scenario 3 — **a client inside a *different* concurrent run fails.** This is the
/// case an environment-variable convention cannot catch: the caller is genuinely
/// contained, genuinely a member of *a* run, and still not a member of the one it
/// asked about. Both runs are live at the same instant, so the answer cannot come
/// from one of them having ended.
#[test]
fn a_client_inside_a_different_concurrent_run_is_not_a_member() {
    let dir = scratch("attest-other-run");
    let registry = registry_dir(&dir);
    let child = spawn_live_run(&dir, &registry, "run-a", "run-a");

    let out = run_attesting(&dir, &registry, "run-b", "run-b", "run-a");
    let value = attestation(
        &out,
        NOT_A_MEMBER,
        "a client contained by run-b attesting run-a",
    );
    assert_eq!(value["verdict"], "not_a_member");
    assert_eq!(
        value["run_id"], "run-a",
        "the verdict is attributed to the run that was asked, never to the caller's own"
    );

    // And the very same process *is* a member of its own run — the difference is the
    // run named on the command line, nothing about the caller.
    let out = run_attesting(&dir, &registry, "run-c", "run-c", "run-c");
    assert_eq!(
        attestation(&out, 0, "a client attesting its own run")["verdict"],
        "member"
    );

    cancel_run(&registry, "run-a", child);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Scenario 4 — **a stale or duplicated run id stays distinguishable from a real
/// negative.** Three outcomes that a single boolean would flatten into "no": the
/// runner is gone, the id names more than one live run, and the id names nothing at
/// all. None of them may borrow `not_a_member`'s code — that one means the runner
/// answered.
#[test]
fn a_stale_or_duplicated_run_id_stays_distinguishable_from_a_real_negative() {
    let dir = scratch("attest-ambiguous");
    let registry = registry_dir(&dir);
    std::fs::create_dir_all(&registry).expect("create the scratch registry");

    // Confirmed stale: a record whose runner is gone.
    write_stale_entry(&registry, "gone-42", "gone-42");
    let stale = Command::new(bin())
        .args(["attest", "--run-id", "gone-42", "--error-format", "json"])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", &registry)
        .output()
        .expect("spawn the attest client");
    assert_eq!(
        stale.status.code(),
        Some(CONTROL),
        "a gone runner is an unreachable target, never a membership verdict"
    );
    let envelope: Value = serde_json::from_str(String::from_utf8_lossy(&stale.stderr).trim())
        .expect("the envelope is one JSON line");
    assert_eq!(envelope["kind"], "stale");
    assert!(
        stale.stdout.is_empty(),
        "no attestation is printed when none was made: {}",
        String::from_utf8_lossy(&stale.stdout)
    );

    // Nothing at all under that id — again not a verdict about membership.
    let missing = Command::new(bin())
        .args([
            "attest",
            "--run-id",
            "never-existed",
            "--error-format",
            "json",
        ])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", &registry)
        .output()
        .expect("spawn the attest client");
    assert_eq!(missing.status.code(), Some(CONTROL));
    let envelope: Value = serde_json::from_str(String::from_utf8_lossy(&missing.stderr).trim())
        .expect("the envelope is one JSON line");
    assert_eq!(envelope["kind"], "not_found");

    // Two live runs share one id: the client refuses to pick, rather than attesting
    // against whichever the directory scan returned first — a wrong "member" here
    // would be a membership claim about the wrong container.
    let first = spawn_live_run(&dir, &registry, "dup-42", "dup-1");
    let second = runner(&dir, &registry, "dup-42", "dup-2")
        .arg("--")
        .args(long_child())
        .spawn()
        .expect("spawn the duplicate run");
    wait_until(
        || published_endpoints(&registry, "dup-42") >= 2,
        Duration::from_secs(20),
        "two live records under one run id",
    );

    let ambiguous = Command::new(bin())
        .args(["attest", "--run-id", "dup-42", "--error-format", "json"])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", &registry)
        .output()
        .expect("spawn the attest client");
    assert_eq!(
        ambiguous.status.code(),
        Some(CONTROL),
        "an ambiguous id is refused, never guessed; stderr: {}",
        String::from_utf8_lossy(&ambiguous.stderr)
    );
    let envelope: Value = serde_json::from_str(String::from_utf8_lossy(&ambiguous.stderr).trim())
        .expect("the envelope is one JSON line");
    assert_eq!(envelope["kind"], "ambiguous_run_id");
    assert_ne!(
        ambiguous.status.code(),
        Some(NOT_A_MEMBER),
        "ambiguity must never be reported as a decided non-membership"
    );

    // Tear both duplicates down: `kill --run-id` would refuse the ambiguous id for
    // the same reason, so the aggregate form is what ends them.
    let out = Command::new(bin())
        .args(["kill", "--all"])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", &registry)
        .output()
        .expect("spawn the aggregate kill client");
    assert!(out.status.success());
    for mut runner_process in [first, second] {
        let _ = runner_process.wait();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Scenario 5 — **nested root/leaf runs.** A run started *inside* another run is an
/// ordinary run, and a client inside it attests against it exactly as a client inside
/// a top-level run does. This is the part that is identical on Windows, Linux and
/// macOS, and it is the part an adapter should rely on: ask about the run you mean.
///
/// The other direction — a leaf-run process asking about the *root* run — is
/// deliberately not asserted, because it is genuinely mechanism-dependent rather than
/// unspecified: a Windows Job Object nests, so the inner run's processes are also
/// members of the outer job, while a Linux cgroup leaf is created *inside* the outer
/// run's own cgroup and its processes therefore leave the outer `cgroup.procs` they
/// would otherwise be listed in. Both answers are honest reports of what that
/// mechanism enumerates as its members, which is why the attestation carries
/// `mechanism` and why `docs/control-plane.md` states the rule instead of this test
/// pinning one platform's answer as the contract.
#[test]
fn nested_runs_attest_against_the_container_they_are_directly_in() {
    let dir = scratch("attest-nested");
    let registry = registry_dir(&dir);

    // The outer run's child is another runner, whose child attests against the inner
    // run: two levels of containment, and the question is about the innermost one.
    let inner_jsonl = dir.join("inner.jsonl");
    let mut outer = runner(&dir, &registry, "root-42", "root");
    outer.arg("--").arg(bin()).arg("run");
    #[cfg(windows)]
    outer.arg("--create-no-window");
    let out = outer
        .args(["--jsonl"])
        .arg(&inner_jsonl)
        .args(["--run-id", "leaf-42", "--timeout", "60s", "--"])
        .arg(bin())
        .args(["attest", "--run-id", "leaf-42", "--json"])
        .output()
        .expect("spawn the nested runs");

    let value = attestation(&out, 0, "a leaf-run client attesting its own run");
    assert_eq!(value["verdict"], "member");
    assert_eq!(value["run_id"], "leaf-42");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The identity is not addressable, and that is a property of the command line
/// itself: a caller cannot ask about a process it names, so it cannot obtain a
/// membership claim about anything but itself. Checked through the real binary
/// because this is the surface a consumer actually meets.
#[test]
fn the_caller_cannot_ask_about_another_process() {
    let dir = scratch("attest-no-pid");
    let registry = registry_dir(&dir);
    std::fs::create_dir_all(&registry).expect("create the scratch registry");

    for extra in [
        vec!["--run-id", "any", "--pid", "1"],
        vec!["--run-id", "any", "--process", "1"],
        vec!["--run-id", "any", "--peer-pid", "1"],
        vec!["--all"],
    ] {
        let out = Command::new(bin())
            .arg("attest")
            .args(&extra)
            .env("PROCESSKIT_CLI_REGISTRY_DIR", &registry)
            .output()
            .expect("spawn the attest client");
        assert_eq!(
            out.status.code(),
            Some(USAGE),
            "`attest {}` must be a parse-time refusal: {}",
            extra.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
