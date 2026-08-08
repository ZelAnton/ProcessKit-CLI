//! Through-the-binary tests for the `doctor` runtime qualification — the
//! side-effecting counterpart of `probe`.
//!
//! Every case here drives the **built binary** (`AGENTS.md`, "Testing tiers") and
//! lets it do the real thing: create a registry, contain a process, round-trip the
//! control plane, and clean up. That is the whole point of the command, so a test
//! that stubbed any of it would be testing something else.
//!
//! Two isolations make that safe to do in a test tier:
//!
//! - `PROCESSKIT_CLI_REGISTRY_DIR` points every invocation at its own scratch
//!   registry, so a qualification never touches (or is confused by) the developer's
//!   own runs; and
//! - `TMPDIR`/`TMP`/`TEMP` point its **scratch directory** into a directory this test
//!   owns, which is what lets the cleanup claims be checked from outside rather than
//!   taken from the report that makes them. A report saying `scratch_removed: true`
//!   proves nothing on its own; an empty directory that the command demonstrably used
//!   does.
//!
//! Coverage mirrors the contract:
//!
//! - a healthy host qualifies, and every cleanup claim is verified independently —
//!   including that something really was contained (`members_before`), so "nothing
//!   remained" is not the vacuous truth of an empty container (K-059);
//! - a `--require-*` expectation changes the **exit code and nothing else**: the
//!   observed facts are byte-identical with and without it;
//! - a host that cannot do its job is reported as unqualified and **keeps** the named
//!   diagnostics directory, with the report inside it;
//! - the scratch child is harmless: it writes nothing, opens no registry, and is
//!   bounded; and
//! - it cannot silently replace a qualification a caller asked for.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

use common::{bin, scratch};
use serde_json::Value;

/// The reserved exit code for a host that did not qualify (`docs/exit-codes.md`).
const HOST_UNQUALIFIED: i32 = 116;
/// The reserved exit code for a malformed command line.
const USAGE: i32 = 100;
/// The reserved exit code a control-plane `cancel` ends a run with — what a healthy
/// qualification's own scratch run must exit with, since the cancel it sent is what
/// ended it.
const CONTROL_CANCELLED: i64 = 108;

/// The three phases every qualification must run, in order, plus the three that
/// follow them. Named here so a test asserts the *sequence* rather than a count.
const MANDATORY_PHASES: &[&str] = &[
    "registry",
    "launch",
    "inspect",
    "cancel",
    "terminal_wait",
    "cleanup",
];

/// A scratch workspace for one test: an isolated registry directory, and an isolated
/// temp directory the `doctor` under test will place its own scratch directory in.
struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(tag: &str) -> Self {
        let root = scratch(tag);
        std::fs::create_dir_all(root.join("tmp")).expect("create the isolated temp dir");
        Self { root }
    }

    fn registry(&self) -> PathBuf {
        self.root.join("registry")
    }

    /// The directory `doctor`'s own scratch directory is created inside, because this
    /// is what `std::env::temp_dir()` resolves to for the invocations below.
    fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Run the built binary's `doctor` with `extra` arguments.
    fn doctor(&self, extra: &[&str]) -> Output {
        let tmp = self.tmp();
        Command::new(bin())
            .arg("doctor")
            .args(extra)
            .env("PROCESSKIT_CLI_REGISTRY_DIR", self.registry())
            // `std::env::temp_dir()` reads `TMPDIR` on unix and `TMP`/`TEMP` on
            // Windows; setting all three makes the scratch directory land where this
            // test can inspect it on either platform.
            .env("TMPDIR", &tmp)
            .env("TMP", &tmp)
            .env("TEMP", &tmp)
            .output()
            .expect("spawn the doctor")
    }

    /// The entries left in the isolated temp directory — empty on every path that
    /// does not deliberately keep a diagnostics directory.
    fn leftover_temp_entries(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.tmp())
            .expect("read the isolated temp dir")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect()
    }

    /// The registry records left behind — empty once a qualification has cleaned up
    /// after itself.
    fn registry_records(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.registry()) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect()
    }

    fn cleanup(self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Assert the exit code and parse the single JSON report line `doctor --json` prints.
fn report(out: &Output, code: i32, what: &str) -> Value {
    assert_eq!(
        out.status.code(),
        Some(code),
        "{what} must exit {code}; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    parse_report(out, what)
}

/// Parse that report **without** judging the exit code — for the one case whose
/// expected code is decided by the report itself (an optional check that may or may
/// not reach a verdict on the host running these tests).
fn parse_report(out: &Output, what: &str) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_else(|| panic!("{what} prints a JSON report line; got stdout {stdout:?}"));
    serde_json::from_str(line)
        .unwrap_or_else(|err| panic!("{what}'s report is valid JSON: {err}; line {line:?}"))
}

/// The phase names a report carries, in order.
fn phase_names(report: &Value) -> Vec<String> {
    report["phases"]
        .as_array()
        .expect("phases is an array")
        .iter()
        .map(|phase| {
            phase["phase"]
                .as_str()
                .expect("each phase names itself")
                .to_owned()
        })
        .collect()
}

/// A healthy host qualifies, and every claim the report makes about cleanup is
/// checked **from outside it**: the registry it created holds no leftover record, and
/// the temp directory it worked in is empty.
///
/// The container facts are asserted with the same suspicion: `members_before` and
/// `inspected_members` must be non-zero, so "teardown left nothing" is a fact about a
/// container that really held a process rather than the vacuous truth about an empty
/// one (K-059). The run's terminal code pins *what* ended it — the cancel this
/// qualification sent, not a timeout or a child that simply finished.
#[test]
fn a_healthy_host_qualifies_and_leaves_nothing_behind() {
    let workspace = Workspace::new("doctor-healthy");
    let out = workspace.doctor(&["--json"]);
    let report = report(&out, 0, "`doctor --json` on this host");

    assert_eq!(report["qualified"], true, "{report}");
    assert_eq!(report["doctor_version"], 1);
    assert_eq!(report["binary"], "processkit-cli");
    assert_eq!(report["os"], std::env::consts::OS);
    assert_eq!(report["failures"].as_array().map(Vec::len), Some(0));
    assert_eq!(report["mismatches"].as_array().map(Vec::len), Some(0));
    assert!(report["diagnostics_dir"].is_null(), "{report}");

    // Registry: the directory this invocation was pointed at, confirmed owner-only by
    // re-reading it rather than by the create having returned.
    let registry = &report["registry"];
    assert_eq!(registry["owner_only"], true, "{report}");
    assert!(
        Path::new(registry["dir"].as_str().expect("the registry dir is named"))
            .ends_with("registry"),
        "the report names the registry it actually used: {report}"
    );
    let protection = registry["protection"].as_str().expect("a protection name");
    assert_eq!(
        protection,
        if cfg!(windows) {
            "windows_owner_only_dacl"
        } else {
            "posix_0700"
        },
        "the protection names this platform's own mechanism: {report}"
    );

    // Containment: a real mechanism, a real contained PID.
    let containment = &report["containment"];
    let mechanism = containment["mechanism"].as_str().expect("a mechanism");
    assert!(
        [
            "job_object",
            "cgroup_v2",
            "process_group",
            "process_reaper",
            "unknown",
        ]
        .contains(&mechanism),
        "the mechanism is one this project publishes: {report}"
    );
    let abrupt = containment["abrupt_cleanup"].as_str().expect("a level");
    assert!(
        ["whole_tree", "direct_child_only", "none"].contains(&abrupt),
        "the abrupt-cleanup level is one this project publishes: {report}"
    );
    assert!(
        containment["root_pid"].as_u64().is_some_and(|pid| pid > 0),
        "a real child was contained: {report}"
    );

    // Control: the round-trip really carried content, and the run really ended
    // because of the cancel this qualification sent.
    let control = &report["control"];
    assert!(
        control["inspected_members"]
            .as_u64()
            .is_some_and(|members| members >= 1),
        "the inspect round-trip saw the contained child, so it is not vacuously empty: {report}"
    );
    assert_eq!(control["cancel_acknowledged"], true, "{report}");
    assert_eq!(control["terminal_exit_code"], CONTROL_CANCELLED, "{report}");
    assert_eq!(control["terminal_source"], "control_cancel", "{report}");
    assert_eq!(
        control["transport"],
        if cfg!(windows) {
            "windows_named_pipe"
        } else {
            "unix_socket"
        },
        "{report}"
    );

    // Cleanup: the container held something, the teardown read was real, and every
    // artifact is gone.
    let cleanup = &report["cleanup"];
    assert!(
        cleanup["members_before"]
            .as_u64()
            .is_some_and(|members| members >= 1),
        "teardown began on a container that really held a process: {report}"
    );
    assert_eq!(cleanup["read_error"], false, "{report}");
    assert_eq!(cleanup["registry_record_removed"], true, "{report}");
    assert_eq!(cleanup["endpoint_released"], true, "{report}");
    assert_eq!(cleanup["scratch_removed"], true, "{report}");
    // `confirmed_empty` is the strict reading, and it is the *conclusive* mechanisms
    // that must satisfy it. On the POSIX process-group fallback a post-kill snapshot
    // still lists a just-exited child nobody has reaped, so the report says the
    // snapshot is inconclusive there instead of claiming a survivor — and this test
    // asserts exactly that pairing rather than either half of it.
    if cleanup["teardown_snapshot_conclusive"] == Value::Bool(true) {
        assert_eq!(cleanup["confirmed_empty"], true, "{report}");
        assert_eq!(cleanup["remaining"], 0, "{report}");
    }
    assert_eq!(
        cleanup["teardown_snapshot_conclusive"],
        Value::Bool(mechanism != "process_group"),
        "the snapshot is conclusive exactly off the POSIX fallback: {report}"
    );

    // Every phase ran, in order, and each was timed.
    assert_eq!(phase_names(&report), MANDATORY_PHASES, "{report}");
    for phase in report["phases"].as_array().expect("phases") {
        assert_eq!(phase["ok"], true, "{phase}");
        assert!(phase["detail"].is_null(), "{phase}");
        assert!(phase["elapsed_ms"].is_u64(), "{phase}");
    }
    assert!(report["elapsed_ms"].is_u64(), "{report}");

    // The independent half: what the report claims about cleanup, checked from
    // outside the report.
    assert!(
        workspace.registry_records().is_empty(),
        "the scratch run's registry record must be gone: {:?}",
        workspace.registry_records()
    );
    assert!(
        workspace.leftover_temp_entries().is_empty(),
        "a qualified host leaves no scratch directory behind: {:?}",
        workspace.leftover_temp_entries()
    );

    workspace.cleanup();
}

/// The report is facts; a `--require-*` flag is a gate over them. Two invocations
/// that differ only in a requirement produce the **same observed facts** and differ
/// only in `qualified`, `mismatches`, and the exit code.
///
/// Differential rather than absence-based (K-059): the requirement is checked in both
/// directions against the *same* host — satisfied with the mechanism this host really
/// reports (exit `0`), unsatisfiable with one no platform reports (exit `116`) — so a
/// flag that did nothing at all would fail the second half, and a flag that suppressed
/// or altered the report would fail the comparison.
#[test]
fn a_requirement_gates_the_exit_code_and_nothing_else() {
    let workspace = Workspace::new("doctor-requirement");

    let baseline = report(
        &workspace.doctor(&["--json"]),
        0,
        "the baseline qualification",
    );
    let mechanism = baseline["containment"]["mechanism"]
        .as_str()
        .expect("the baseline observed a mechanism")
        .to_owned();

    // Satisfied: the same host, asked for what it actually is.
    let satisfied = report(
        &workspace.doctor(&["--json", "--require-mechanism", &mechanism]),
        0,
        "a satisfied requirement",
    );
    assert_eq!(satisfied["qualified"], true, "{satisfied}");
    assert_eq!(satisfied["mismatches"].as_array().map(Vec::len), Some(0));

    // Unsatisfiable: the same host, asked for something no platform reports.
    let refused = report(
        &workspace.doctor(&["--json", "--require-mechanism", "no-such-mechanism"]),
        HOST_UNQUALIFIED,
        "an unmeetable requirement",
    );
    assert_eq!(refused["qualified"], false, "{refused}");
    let mismatches = refused["mismatches"]
        .as_array()
        .expect("mismatches is an array");
    assert_eq!(mismatches.len(), 1, "{refused}");
    let text = mismatches[0].as_str().expect("a mismatch reason");
    assert!(
        text.contains("no-such-mechanism") && text.contains(&mechanism),
        "the mismatch names both what was required and what this host is: {text}"
    );

    // An unmet requirement is not a failed phase, and nothing is kept for it: the
    // host did everything asked of it, it just is not the host that was wanted.
    assert_eq!(refused["failures"].as_array().map(Vec::len), Some(0));
    assert!(refused["diagnostics_dir"].is_null(), "{refused}");
    assert!(
        workspace.leftover_temp_entries().is_empty(),
        "a mismatch keeps no diagnostics: {:?}",
        workspace.leftover_temp_entries()
    );

    // The facts themselves are identical across all three invocations. Compared field
    // by field over everything that is not a per-run value (a pid, an endpoint, a
    // path, a duration), because those legitimately differ between two runs of the
    // same command on the same host.
    //
    // The per-run values that exclusion leaves out are `containment.root_pid`,
    // `control.endpoint`, `cleanup.remaining_pids`, and every `elapsed_ms`.
    // `cleanup.remaining_pids` is the least obvious of them, so it is named here: on
    // the POSIX `process_group` fallback the post-kill snapshot is a `kill(pid, 0)`
    // probe that a just-exited, unreaped child still answers (`docs/schema.md`,
    // "cleanup_finished"), so every invocation contributes its own freshly assigned pid
    // to it. What a requirement flag could actually change about that field is its
    // *size*, and that is compared below as `cleanup.remaining` — the count of exactly
    // those pids (`src/run/teardown.rs`'s `emit_cleanup_finished`).
    //
    // `registry.dir` is compared despite being a path because this test pins it through
    // the environment: it is the same configured directory every time, not a per-run
    // one the command chose.
    for other in [&satisfied, &refused] {
        assert_eq!(
            other["registry"]["owner_only"], baseline["registry"]["owner_only"],
            "the observed registry protection must not depend on a requirement flag"
        );
        assert_eq!(
            other["registry"]["protection"],
            baseline["registry"]["protection"]
        );
        assert_eq!(other["registry"]["dir"], baseline["registry"]["dir"]);
        assert_eq!(
            other["containment"]["mechanism"], baseline["containment"]["mechanism"],
            "the observed mechanism must not depend on a requirement flag"
        );
        assert_eq!(
            other["containment"]["abrupt_cleanup"],
            baseline["containment"]["abrupt_cleanup"]
        );
        assert_eq!(
            other["control"]["transport"],
            baseline["control"]["transport"]
        );
        assert_eq!(
            other["control"]["cancel_acknowledged"],
            baseline["control"]["cancel_acknowledged"]
        );
        assert_eq!(
            other["control"]["terminal_exit_code"],
            baseline["control"]["terminal_exit_code"]
        );
        assert_eq!(
            other["control"]["terminal_source"],
            baseline["control"]["terminal_source"]
        );
        for field in [
            "read_error",
            "confirmed_empty",
            "teardown_snapshot_conclusive",
            "registry_record_removed",
            "endpoint_released",
            "scratch_removed",
            "remaining",
        ] {
            assert_eq!(
                other["cleanup"][field], baseline["cleanup"][field],
                "cleanup.{field} must not depend on a requirement flag"
            );
        }
        assert_eq!(
            other["resource_controller"], baseline["resource_controller"],
            "an unrelated requirement must not conjure the optional check"
        );
        assert_eq!(phase_names(other), phase_names(&baseline));
        assert_eq!(
            other["failures"], baseline["failures"],
            "a requirement is not a failure"
        );
    }

    workspace.cleanup();
}

/// A host that cannot even create its run registry is reported as unqualified — with
/// the phase that failed named, every later fact honestly `null` rather than
/// fabricated, and the diagnostics directory **kept** at the path the report names.
///
/// The failure is forced the same way on both platforms: point the registry at a path
/// whose parent is a regular file, so the directory cannot be created at all
/// (`ENOTDIR` on unix, `ERROR_ALREADY_EXISTS` on Windows). That is a real environment
/// failure of exactly the kind `doctor` exists to catch before a production run does.
#[test]
fn a_host_that_cannot_create_its_registry_keeps_the_named_evidence() {
    let workspace = Workspace::new("doctor-broken-registry");
    let blocker = workspace.root.join("not-a-directory");
    std::fs::write(&blocker, b"this is a file, not a registry parent")
        .expect("write the blocking file");
    let tmp = workspace.tmp();

    let out = Command::new(bin())
        .args(["doctor", "--json"])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", blocker.join("registry"))
        .env("TMPDIR", &tmp)
        .env("TMP", &tmp)
        .env("TEMP", &tmp)
        .output()
        .expect("spawn the doctor");
    let report = report(
        &out,
        HOST_UNQUALIFIED,
        "`doctor` against an unusable registry",
    );

    assert_eq!(report["qualified"], false, "{report}");
    assert_eq!(phase_names(&report), ["registry"], "{report}");
    assert_eq!(report["phases"][0]["ok"], false, "{report}");
    assert!(
        report["phases"][0]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("registry")),
        "the failed phase says what went wrong: {report}"
    );
    let failures = report["failures"].as_array().expect("failures is an array");
    assert_eq!(failures.len(), 1, "{report}");
    assert_eq!(report["mismatches"].as_array().map(Vec::len), Some(0));

    // Nothing after the failing phase is claimed — an unobserved fact is null, never
    // a default that would read as an observation.
    for absent in ["registry", "containment", "control", "cleanup"] {
        assert!(
            report[absent].is_null(),
            "`{absent}` was never observed and must not be reported: {report}"
        );
    }

    // The evidence is kept, and the report names where.
    let diagnostics = report["diagnostics_dir"]
        .as_str()
        .unwrap_or_else(|| panic!("a failed qualification names its diagnostics dir: {report}"));
    let diagnostics = PathBuf::from(diagnostics);
    assert!(
        diagnostics.is_dir(),
        "the named diagnostics directory really exists: {}",
        diagnostics.display()
    );
    let kept = diagnostics.join("doctor-report.json");
    assert!(
        kept.is_file(),
        "the report travels with its own evidence: {}",
        kept.display()
    );
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(&kept).expect("read the copy"))
        .expect("the kept copy is valid JSON");
    assert_eq!(
        kept["failures"], report["failures"],
        "the kept copy is the same verdict the caller was given"
    );
    assert_eq!(
        workspace.leftover_temp_entries(),
        vec![diagnostics],
        "exactly the named directory is kept, and nothing else"
    );

    // The stderr line points at the same place, for the operator who is not parsing
    // JSON.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("diagnostics kept in"),
        "the failure line names the kept evidence: {stderr}"
    );

    workspace.cleanup();
}

/// The scratch child is exactly what it claims to be: it sleeps for its bounded
/// duration, exits `0`, writes nothing anywhere, and — the part worth checking rather
/// than assuming — never opens a registry, even when pointed at one.
#[test]
fn the_scratch_child_is_harmless_and_bounded() {
    let workspace = Workspace::new("doctor-scratch-child");
    let cwd = workspace.root.join("cwd");
    std::fs::create_dir_all(&cwd).expect("create the child's working directory");
    let registry = workspace.root.join("never-created");

    let started = Instant::now();
    let out = Command::new(bin())
        .args(["doctor", "--scratch-child", "300ms"])
        .current_dir(&cwd)
        .env("PROCESSKIT_CLI_REGISTRY_DIR", &registry)
        .output()
        .expect("spawn the scratch child");
    let elapsed = started.elapsed();

    assert_eq!(
        out.status.code(),
        Some(0),
        "the scratch child exits cleanly"
    );
    assert!(out.stdout.is_empty(), "it prints nothing on stdout");
    assert!(
        out.stderr.is_empty(),
        "it prints nothing on stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(250),
        "it really waits out its duration rather than returning at once: {elapsed:?}"
    );
    assert!(
        !registry.exists(),
        "the scratch child opens no registry, even when one is configured"
    );
    let leftovers: Vec<_> = std::fs::read_dir(&cwd)
        .expect("read the child's working directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "it writes nothing to its working directory: {leftovers:?}"
    );

    workspace.cleanup();
}

/// A qualification a caller asked for can never be silently replaced by a sleep: the
/// report-replacing flag conflicts with every other one at the clap level, so the
/// combination is an ordinary `USAGE` (100) refusal — checked here through the real
/// binary, since the guarantee is about what the *process* does (the same structural
/// refusal `probe --print-schema` uses, K-076).
#[test]
fn the_scratch_child_cannot_replace_a_requested_qualification() {
    let workspace = Workspace::new("doctor-scratch-child-conflict");
    for extra in [
        vec![
            "--scratch-child",
            "1s",
            "--require-abrupt-cleanup",
            "whole_tree",
        ],
        vec!["--scratch-child", "1s", "--require-mechanism", "job_object"],
        vec!["--scratch-child", "1s", "--json"],
        vec!["--scratch-child", "1s", "--check-resource-controller"],
    ] {
        let out = workspace.doctor(&extra);
        assert_eq!(
            out.status.code(),
            Some(USAGE),
            "{extra:?} must be refused as a usage error, never accepted as a sleep; stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            out.stdout.is_empty(),
            "a refused invocation prints no report: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    workspace.cleanup();
}

/// The optional resource-controller check is absent unless asked for — `null` meaning
/// "nothing was established", never "not available" — and asking for it never fails a
/// host by itself. Only `--require-resource-controller` turns its answer into a
/// verdict, and this asserts that pairing in whichever direction this host actually
/// goes, rather than hard-coding an availability that differs per platform.
///
/// The pairing asserted here is the one the module's own invariant rests on: the facts
/// are published **exactly** when the phase reached a verdict. A check that could not
/// be performed at all — a scratch run that ended for some reason other than the cap,
/// a budget that ran out — fails its phase and leaves `resource_controller` `null`,
/// never `available: false`, because "this host cannot enforce a cap" and "nobody found
/// out" are different answers. Which of the two branches this host takes is a property
/// of the platform; that the report and the phase agree is not
/// (`src/doctor.rs`'s `classify_resource_outcome` tests drive every ending directly).
#[test]
fn the_resource_controller_check_is_opt_in_and_only_a_requirement_gates_it() {
    let workspace = Workspace::new("doctor-resources");

    let bare = report(&workspace.doctor(&["--json"]), 0, "a bare qualification");
    assert!(
        bare["resource_controller"].is_null(),
        "an unrequested check reports nothing at all: {bare}"
    );
    assert!(
        !phase_names(&bare).contains(&"resource_controller".to_string()),
        "and runs no phase: {bare}"
    );

    let out = workspace.doctor(&["--json", "--check-resource-controller"]);
    let checked = parse_report(&out, "a requested resource-controller check");
    let phases = checked["phases"].as_array().expect("phases is an array");
    let phase = phases
        .last()
        .expect("a requested check runs a phase")
        .clone();
    assert_eq!(
        phase["phase"], "resource_controller",
        "the optional phase runs after every mandatory one: {checked}"
    );

    let facts = &checked["resource_controller"];
    let verdict = facts["available"].as_bool();
    assert_eq!(
        verdict.is_some(),
        phase["ok"] == Value::Bool(true),
        "the verdict is published exactly when the phase reached one — never a negative \
         on a check that could not be performed: {checked}"
    );
    assert_eq!(
        out.status.code(),
        Some(if verdict.is_some() {
            0
        } else {
            HOST_UNQUALIFIED
        }),
        "a check that reached a verdict fails nothing; one that could not be performed is a \
         failed phase: {checked}"
    );

    match verdict {
        Some(available) => {
            assert!(
                facts["requested"]
                    .as_str()
                    .is_some_and(|requested| requested.contains("--max-processes")),
                "it names the cap it asked for, as the flag that asks for it: {checked}"
            );
            assert_eq!(
                facts["detail"].is_null(),
                available,
                "an unavailable controller says why — from the scratch run's own `limit_hit` \
                 event — and an available one has nothing to explain: {checked}"
            );
            assert_eq!(
                checked["qualified"], true,
                "the check alone never fails a host — only a requirement does: {checked}"
            );
        }
        None => {
            assert!(
                facts.is_null(),
                "a check that reached no verdict publishes none: {checked}"
            );
            assert!(
                phase["detail"]
                    .as_str()
                    .is_some_and(|detail| !detail.is_empty()),
                "and its failed phase says what stopped it: {checked}"
            );
        }
    }

    // Now the requirement, in whichever direction this host goes. It is met exactly
    // when the controller was *observed* available — an unestablished fact is a
    // mismatch too, on the honest ground that nothing was observed rather than on a
    // negative nobody proved.
    let required = workspace.doctor(&[
        "--json",
        "--check-resource-controller",
        "--require-resource-controller",
    ]);
    let met = verdict == Some(true);
    let expected = if met { 0 } else { HOST_UNQUALIFIED };
    let required = report(&required, expected, "a required resource controller");
    assert_eq!(required["qualified"], met, "{required}");
    assert_eq!(
        required["mismatches"].as_array().map(Vec::len),
        Some(usize::from(!met)),
        "the requirement is unmet exactly when the controller was not observed available: \
         {required}"
    );

    // And the requirement cannot be asked about a fact that was never observed.
    let unobserved = workspace.doctor(&["--json", "--require-resource-controller"]);
    assert_eq!(
        unobserved.status.code(),
        Some(USAGE),
        "requiring the controller without checking it is a usage error, not a guess"
    );

    workspace.cleanup();
}

/// The default rendering is for a human and carries the same facts the JSON does —
/// including the verdict, the mechanism, and the registry it used.
#[test]
fn the_human_rendering_carries_the_same_facts() {
    let workspace = Workspace::new("doctor-human");
    let out = workspace.doctor(&[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).expect("the rendering is UTF-8");
    assert!(text.contains("qualified"), "{text}");
    for expected in [
        "registry:",
        "containment:",
        "control:",
        "cleanup:",
        "phases:",
    ] {
        assert!(
            text.contains(expected),
            "the rendering names `{expected}`: {text}"
        );
    }
    // The same JSON facts, in prose: the run really ended by the cancel, and cleanup
    // really was confirmed.
    assert!(
        text.contains("control_cancel"),
        "the rendering says what ended the scratch run: {text}"
    );
    assert!(
        workspace.leftover_temp_entries().is_empty(),
        "the human path cleans up exactly like the JSON one"
    );
    workspace.cleanup();
}
