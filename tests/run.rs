//! Through-the-binary tests for the `run` subcommand: exit-code fidelity, live
//! stream pass-through with strict separation, the spawn-failure code, timeout
//! and stop-signal cancel (`Ctrl-C`; on Unix `SIGTERM`/`SIGHUP`; on Windows
//! `Ctrl-Break`) as distinguishable runner-imposed endings, the `--grace`
//! pause, and kernel-backed teardown of a leaked descendant. These prove behavior
//! the library-level ProcessKit-rs suite cannot: the *binary's* own contracts
//! (`AGENTS.md`, "Testing tiers"). The full end-to-end scenario matrix is a
//! separate task (T-010); this is the base proof through the shipped binary.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{bin, events_path, run, run_with_flags, scratch, shell_inline};
use serde_json::Value;

/// The core rule: a completed run forwards the child's exact code (see
/// `docs/exit-codes.md`). Zero stays zero.
#[test]
fn forwards_a_zero_exit_code() {
    let dir = scratch("exit0");
    let out = run(&dir, &[], shell_inline("exit 0"));
    assert_eq!(out.status.code(), Some(0), "a clean child must exit 0");
    let events = read_run_events(&dir);
    assert!(
        !events
            .iter()
            .any(|event| event["event"] == "limit_evidence"),
        "a run without a requested cap emits no limit_evidence: {events:?}"
    );
}

/// A non-zero child code is forwarded verbatim — not clamped, not aliased onto a
/// runner-own code.
#[test]
fn forwards_a_nonzero_exit_code() {
    let dir = scratch("exit7");
    let out = run(&dir, &[], shell_inline("exit 7"));
    assert_eq!(
        out.status.code(),
        Some(7),
        "the child's code must pass through unchanged"
    );
}

/// Test-only console policy is explicit: ordinary Windows fixtures suppress a
/// delegated terminal pane, while the CTRL_BREAK proof can still opt into the
/// caller's real console.
#[cfg(windows)]
#[test]
fn windows_headless_fixtures_request_no_console_unless_explicitly_inherited() {
    let dir = scratch("windows-test-console-policy");
    let headless = common::command_with_flags(&dir, &[], &[], shell_inline("exit 0"));
    assert!(
        headless.get_args().any(|arg| arg == "--create-no-window"),
        "ordinary Windows fixtures must not create delegated terminal panes"
    );

    let inherited =
        common::command_with_inherited_console_flags(&dir, &[], &[], shell_inline("exit 0"));
    assert!(
        inherited.get_args().all(|arg| arg != "--create-no-window"),
        "the real CTRL_BREAK proof must retain the caller's console"
    );

    let direct = common::headless_run_command();
    assert!(
        direct.get_args().any(|arg| arg == "--create-no-window"),
        "incrementally assembled Windows fixtures need the same headless policy"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Child stdout and stderr are echoed live and stay strictly separated — child
/// stdout to our stdout, child stderr to our stderr — and no runner diagnostic
/// ever leaks into the child's stdout (`AGENTS.md`, "Streams are strictly
/// separated").
#[test]
fn passes_child_streams_through_without_mixing() {
    let dir = scratch("streams");
    let script = if cfg!(windows) {
        "echo OUT&echo ERR 1>&2"
    } else {
        "echo OUT; echo ERR 1>&2"
    };
    let out = run(&dir, &[], shell_inline(script));
    assert!(out.status.success(), "the child exits cleanly");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(stdout.contains("OUT"), "child stdout reaches our stdout");
    assert!(
        !stdout.contains("ERR"),
        "child stderr must not bleed into our stdout: {stdout:?}"
    );
    assert!(stderr.contains("ERR"), "child stderr reaches our stderr");
    assert!(
        !stdout.contains("processkit-cli"),
        "no runner diagnostic may appear on the child's stdout: {stdout:?}"
    );
}

/// `--inherit-stdin` gives the child the runner's input handle without changing
/// the output or lifecycle contracts. The parent pipe makes this deterministic on
/// Windows and Unix while exercising the same inheritance mode a terminal uses.
#[test]
fn inherited_stdin_reaches_the_child_and_preserves_the_terminal_event() {
    let dir = scratch("inherit-stdin");
    let mut runner =
        common::command_with_flags(&dir, &[], &["--inherit-stdin"], stdin_reader_program(&dir))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the runner with a piped stdin");
    let mut stdin = runner
        .stdin
        .take()
        .expect("the runner receives the test pipe");
    stdin
        .write_all(b"inherited line\n")
        .expect("write one line for the child");
    drop(stdin);

    let out = runner
        .wait_with_output()
        .expect("the runner exits after the child reads stdin");
    assert_eq!(out.status.code(), Some(0), "child exit is forwarded");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("stdin:inherited line"),
        "the child read the inherited line: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_child_exit_event(&dir);

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--inherit-stdio` hands all three outer handles directly to the child. Piping
/// the runner from this parent makes the contract deterministic on Windows and
/// Unix: the child reads from the same input pipe and writes to the same two
/// output pipes, while lifecycle data remains isolated in JSONL.
#[test]
fn inherited_stdio_reaches_the_child_directly_and_preserves_the_terminal_event() {
    let dir = scratch("inherit-stdio");
    let mut runner =
        common::command_with_flags(&dir, &[], &["--inherit-stdio"], stdio_reader_program(&dir))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the runner with three piped outer handles");
    let mut stdin = runner
        .stdin
        .take()
        .expect("the runner receives the test input pipe");
    stdin
        .write_all(b"direct line\n")
        .expect("write one line through inherited stdin");
    drop(stdin);

    let out = runner
        .wait_with_output()
        .expect("the runner exits after the child reads inherited stdio");
    assert_eq!(out.status.code(), Some(0), "child exit is forwarded");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("stdio-out:direct line"),
        "the child writes directly to inherited stdout: {stdout:?}"
    );
    assert!(
        stderr.contains("stdio-err:direct line"),
        "the child writes directly to inherited stderr: {stderr:?}"
    );

    assert_child_exit_event(&dir);
    let events = read_run_events(&dir);
    assert!(
        !events
            .iter()
            .any(|event| event["event"] == "output_captured"),
        "interactive output must never be copied into lifecycle JSONL"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--stdin-file` keeps input bytes out of the command tail and streams them via
/// ProcessKit, closing the child's stdin once the file reaches EOF.
#[test]
fn stdin_file_reaches_the_child_and_preserves_the_terminal_event() {
    let dir = scratch("stdin-file");
    let input = dir.join("input.txt");
    std::fs::write(&input, b"file line\n").expect("write stdin fixture");
    let input_flag = path_arg(&input);

    let out = run_with_flags(
        &dir,
        &[],
        &["--stdin-file", &input_flag],
        stdin_reader_program(&dir),
    );
    assert_eq!(out.status.code(), Some(0), "child exit is forwarded");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("stdin:file line"),
        "the child read the file's line: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_child_exit_event(&dir);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A missing input file fails before the command starts, so the child cannot
/// accidentally run with a different stdin mode than the caller requested.
#[test]
fn missing_stdin_file_is_a_pre_run_setup_failure() {
    let dir = scratch("stdin-file-missing");
    let missing = path_arg(&dir.join("does-not-exist.txt"));

    let out = run_with_flags(
        &dir,
        &[],
        &["--stdin-file", &missing],
        shell_inline("echo child-must-not-start"),
    );
    assert_eq!(out.status.code(), Some(111));
    assert!(out.stdout.is_empty(), "no child output may be forwarded");

    let events = read_run_events(&dir);
    assert!(
        !events.iter().any(|event| event["event"] == "run_started"),
        "the child must not start when stdin setup fails"
    );
    let terminal = events.last().expect("terminal runner_exit event");
    assert_eq!(terminal["event"], "runner_exit");
    assert_eq!(terminal["source"], "setup");
    assert_eq!(terminal["code"], 111);
    assert!(terminal["child_code"].is_null());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A malformed environment file is a fail-closed setup error and never spawns the
/// child with a partially applied environment.
#[test]
fn malformed_env_file_is_a_pre_run_setup_failure() {
    let dir = scratch("env-file-malformed");
    let env_file = dir.join("bad.env");
    std::fs::write(&env_file, "GOOD=value\nBAD KEY=value\n").expect("write fixture");
    let env_file = path_arg(&env_file);

    let out = run_with_flags(
        &dir,
        &[],
        &["--env-file", &env_file],
        shell_inline("echo child-must-not-start"),
    );
    assert_eq!(out.status.code(), Some(111));
    assert!(out.stdout.is_empty(), "no child output may be forwarded");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("line 2"),
        "the diagnostic identifies the malformed line"
    );

    let events = read_run_events(&dir);
    assert!(!events.iter().any(|event| event["event"] == "run_started"));
    let terminal = events.last().expect("terminal runner_exit event");
    assert_eq!(terminal["source"], "setup");
    assert_eq!(terminal["code"], 111);
    assert!(terminal["child_code"].is_null());

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// `--run-id-env <KEY>` (T-304): publish the run's final id into the child
// environment.
//
// These fixtures use their own throwaway registry (`PROCESSKIT_CLI_REGISTRY_DIR`)
// for the same reason the detach ones below do: the registry answer is read while
// the run is still live, so it must not have to be told apart from a developer's
// real runs or a concurrently running test's.
// ---------------------------------------------------------------------------

/// The destination key these scenarios inject into. Deliberately *not* set in the
/// runner's own environment by the identity scenarios: if the injection failed, the
/// child would observe an unset variable rather than an inherited one, so a passing
/// assertion cannot be an accident of inheritance.
///
/// The precedence scenarios further below are the deliberate exception — they *do*
/// pre-set it (to [`RUN_ID_ENV_INHERITED`]) or fill it from an `--env-file` (with
/// [`RUN_ID_ENV_FROM_FILE`]), because `--env-clear`/`--env-remove` need something
/// real to act on. There the same guarantee is kept by a different means: those
/// values are distinctive, and an assertion that the child observed the *run id*
/// fails on either of them just as it fails on an unset variable.
const RUN_ID_ENV_KEY: &str = "PROCESSKIT_TEST_RUN_ID";

/// What the precedence scenarios give [`RUN_ID_ENV_KEY`] in the runner's own
/// environment, so the child inherits it unless a flag says otherwise.
const RUN_ID_ENV_INHERITED: &str = "inherited-value-the-run-id-replaces";

/// What an `--env-file` entry gives [`RUN_ID_ENV_KEY`] in the precedence scenarios.
const RUN_ID_ENV_FROM_FILE: &str = "env-file-value-the-run-id-replaces";

/// How long a `--run-id-env` scenario waits for something the child or the run must
/// do on its own. Generous: it bounds a failure, never a healthy path.
const RUN_ID_ENV_OBSERVE_TIMEOUT: Duration = Duration::from_secs(45);

/// Every independently-sourced answer to "which run is this?" for one scenario.
/// The whole point of the flag is that these cannot disagree.
struct RunIdEvidence {
    /// What the child process actually read out of its own environment.
    child_observed: String,
    /// The child's own stdout line, echoed live by the runner — the "child prints
    /// its environment" half, kept separate from the file handshake above so a
    /// blocked echo path cannot pass as a successful injection.
    child_echoed: String,
    /// `run_started.run_id` from the run's `--jsonl` stream.
    run_started: String,
    /// The `run_id` of the run's per-user registry record, read while it was live.
    registry: String,
    /// The `run_id` a control-plane `inspect` reply carries, from the live runner.
    control_plane: String,
}

/// A child that records the value it observes for [`RUN_ID_ENV_KEY`], echoes it,
/// and then waits for a release marker instead of exiting — so the run is still
/// live while the registry and the control plane are asked about it.
fn write_run_id_observer_script(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        let path = dir.join("observe-run-id.bat");
        // Two deliberate details. The redirection is written *before* `echo`,
        // because in `echo %VAR%>"%FILE%"` cmd would parse a value ending in a digit
        // as a numbered handle redirection (`… 3>file`) — and a generated run id
        // always ends in digits. And the value is written to a temporary name that
        // is then renamed into place, so the waiting test can never read a file that
        // exists but is not written yet (its poll is "non-empty", and a
        // create-then-write would let that be briefly false).
        let body = format!(
            "@echo off\r\n\
             echo run-id-env:%{key}%\r\n\
             >\"%OBSERVED%.tmp\" echo %{key}%\r\n\
             move /y \"%OBSERVED%.tmp\" \"%OBSERVED%\" >nul\r\n\
             :wait\r\n\
             if exist \"%RELEASE%\" goto done\r\n\
             ping -n 2 127.0.0.1 >nul\r\n\
             goto wait\r\n\
             :done\r\n",
            key = RUN_ID_ENV_KEY
        );
        std::fs::write(&path, body).expect("write observe-run-id.bat");
        path
    } else {
        let path = dir.join("observe-run-id.sh");
        // Same write-then-rename handshake as the Windows fixture above, for the
        // same reason.
        let body = format!(
            "#!/bin/sh\n\
             echo \"run-id-env:${key}\"\n\
             printf '%s' \"${key}\" > \"$OBSERVED.tmp\"\n\
             mv \"$OBSERVED.tmp\" \"$OBSERVED\"\n\
             while [ ! -f \"$RELEASE\" ]; do sleep 1; done\n",
            key = RUN_ID_ENV_KEY
        );
        std::fs::write(&path, body).expect("write observe-run-id.sh");
        path
    }
}

/// Drive one `--run-id-env` scenario end to end and collect every answer about the
/// run's identity from a different source.
///
/// The child's file handshake is what makes this deterministic rather than timed:
/// the registry and control-plane queries happen after the child has provably read
/// its environment and before it is released, so all three observations are of the
/// same live run.
fn observe_run_id_env(tag: &str, explicit_run_id: Option<&str>) -> RunIdEvidence {
    let dir = scratch(tag);
    let registry = dir.join("registry");
    let observed = dir.join("observed.txt");
    let release = dir.join("release.marker");
    let script = write_run_id_observer_script(&dir);

    let mut flags = vec!["--run-id-env", RUN_ID_ENV_KEY];
    if let Some(run_id) = explicit_run_id {
        flags.extend(["--run-id", run_id]);
    }
    let runner = common::command_with_flags(
        &dir,
        &[
            ("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path()),
            ("OBSERVED", observed.as_path()),
            ("RELEASE", release.as_path()),
        ],
        &flags,
        script_program(&script),
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn the runner binary");

    wait_until(|| file_len(&observed) > 0, RUN_ID_ENV_OBSERVE_TIMEOUT);
    let child_observed = std::fs::read_to_string(&observed)
        .expect("read what the child observed")
        .trim()
        .to_string();

    // Discovery, while the run is still live: exactly one record exists in this
    // throwaway registry, so there is no ambiguity about whose id this is.
    let listed = cli_against(&registry, &["list", "--json"]);
    assert_eq!(listed.status.code(), Some(0), "list succeeds");
    let listed_out = String::from_utf8_lossy(&listed.stdout).into_owned();
    let entries: Vec<Value> = listed_out
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "the scenario's own registry holds exactly this run: {listed_out:?}"
    );
    assert_eq!(
        entries[0]["health"], "live",
        "the record is read while the run is live, not after it: {}",
        entries[0]
    );
    let registry_run_id = entries[0]["run_id"]
        .as_str()
        .expect("a registry record names its run")
        .to_string();

    // The control plane's own answer, from the live runner rather than from a file.
    let inspected = cli_against(
        &registry,
        &["inspect", "--run-id", &registry_run_id, "--json"],
    );
    assert_eq!(
        inspected.status.code(),
        Some(0),
        "inspect reaches the live runner; stderr: {}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    let snapshot: Value = serde_json::from_slice(&inspected.stdout).expect("inspect prints JSON");
    let control_run_id = snapshot["run_id"]
        .as_str()
        .expect("a control snapshot names its run")
        .to_string();

    // Let the child finish and the run close its own stream.
    std::fs::write(&release, b"go").expect("write the release marker");
    let out = runner
        .wait_with_output()
        .expect("the runner finishes once the child is released");
    assert_eq!(
        out.status.code(),
        Some(0),
        "the released child exits cleanly; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let child_echoed = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("run-id-env:").map(str::to_string))
        .unwrap_or_else(|| panic!("the child echoed the variable it read: {stdout:?}"));

    let events = read_run_events(&dir);
    let run_started = events
        .iter()
        .find(|event| event["event"] == "run_started")
        .unwrap_or_else(|| panic!("the run reported itself started: {events:?}"))["run_id"]
        .as_str()
        .expect("run_started names its run")
        .to_string();

    let _ = std::fs::remove_dir_all(&dir);
    RunIdEvidence {
        child_observed,
        child_echoed,
        run_started,
        registry: registry_run_id,
        control_plane: control_run_id,
    }
}

/// The headline contract of `--run-id-env` (T-304): the child sees the run's
/// **final** id, and every other account of that id agrees with it — for an
/// explicit `--run-id` *and* for one the runner generated.
///
/// The generated case is the one that could not be expressed before this flag: a
/// caller that wanted the child to know its run id had to mint the identity itself
/// and pass it twice (`--run-id <id> --env KEY=<id>`), which foreclosed
/// runner-generated ids entirely, because a generated id was not knowable outside
/// the run until the run had already started.
#[test]
fn run_id_env_gives_the_child_the_final_run_id_explicit_or_generated() {
    let explicit_id = "run-id-env-explicit";
    let explicit = observe_run_id_env("run-id-env-explicit", Some(explicit_id));
    assert_eq!(
        explicit.child_observed, explicit_id,
        "an explicit --run-id reaches the child verbatim"
    );
    assert_run_id_evidence_agrees(&explicit);

    let generated = observe_run_id_env("run-id-env-generated", None);
    assert_run_id_evidence_agrees(&generated);
    assert_ne!(
        generated.child_observed, explicit_id,
        "the generated scenario must not be reading the other scenario's id"
    );
    assert!(
        generated.child_observed.starts_with("run-"),
        "the child observed the runner's own generated id ({}), not something it \
         was given on the command line",
        generated.child_observed
    );
}

/// Every source of the run's identity agreed, and none of them was empty — an
/// all-empty set would otherwise satisfy a naive equality chain.
fn assert_run_id_evidence_agrees(evidence: &RunIdEvidence) {
    assert!(
        !evidence.child_observed.is_empty(),
        "the child observed a value at all"
    );
    assert_eq!(
        evidence.child_observed, evidence.child_echoed,
        "the value the child recorded is the value it printed"
    );
    assert_eq!(
        evidence.child_observed, evidence.run_started,
        "the child's value is the one in run_started.run_id"
    );
    assert_eq!(
        evidence.child_observed, evidence.registry,
        "the child's value is the one in the registry record"
    );
    assert_eq!(
        evidence.child_observed, evidence.control_plane,
        "the child's value is the one the control plane reports"
    );
}

/// The flag is strictly opt-in: a run that does not ask for it injects nothing, so
/// no child inherits a run id it never had before. Differential rather than an
/// absence-only assertion (K-059) — the same key is proven observable in the very
/// same shape of run above.
#[test]
fn without_run_id_env_no_run_id_reaches_the_child_environment() {
    let dir = scratch("run-id-env-absent");
    let out = run(
        &dir,
        &[],
        shell_inline(if cfg!(windows) {
            "echo run-id-env:%PROCESSKIT_TEST_RUN_ID%"
        } else {
            "echo \"run-id-env:$PROCESSKIT_TEST_RUN_ID\""
        }),
    );
    assert_eq!(out.status.code(), Some(0), "the child exits cleanly");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let events = read_run_events(&dir);
    let run_id = events
        .iter()
        .find(|event| event["event"] == "run_started")
        .expect("the run started")["run_id"]
        .as_str()
        .expect("run_started names its run")
        .to_string();
    assert!(
        !stdout.contains(&run_id),
        "no run id may reach a child that never asked for one: {stdout:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The collision decision, through the shipped binary: `--run-id-env KEY` together
/// with an explicit `--env KEY=…` is refused as an ordinary `USAGE` (100) error
/// before anything runs — no child, no events file — and the refusal names the key
/// without ever repeating the value the caller typed beside it.
#[test]
fn run_id_env_colliding_with_an_explicit_env_is_refused_before_the_run_starts() {
    let dir = scratch("run-id-env-collision");
    let secret = "value-that-must-not-reach-diagnostics";
    let entry = format!("{RUN_ID_ENV_KEY}={secret}");
    let out = run_with_flags(
        &dir,
        &[],
        &["--run-id-env", RUN_ID_ENV_KEY, "--env", &entry],
        shell_inline("echo child-must-not-start"),
    );
    assert_eq!(
        out.status.code(),
        Some(100),
        "a contradictory pair is a usage error, not a mid-run surprise; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty(), "no child output may be forwarded");
    assert!(
        !events_path(&dir).exists(),
        "the refusal precedes even the events file"
    );

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains(RUN_ID_ENV_KEY),
        "the refusal names the key it is about: {stderr:?}"
    );
    assert!(
        !stderr.contains(secret),
        "the refusal must not disclose the --env value: {stderr:?}"
    );

    // The same key is accepted the moment the contradiction is removed, so the
    // refusal above is about the *pair*, not about the key or the flag.
    let ok_dir = scratch("run-id-env-collision-resolved");
    let out = run_with_flags(
        &ok_dir,
        &[],
        &["--run-id-env", RUN_ID_ENV_KEY],
        shell_inline("exit 0"),
    );
    assert_eq!(out.status.code(), Some(0), "the flag alone is legal");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&ok_dir);
}

/// Every bare environment-key spelling accepted by `--run-id-env` is also the
/// spelling accepted by `--env-remove`: malformed values fail as `USAGE` before
/// the runner creates its event stream or starts the requested child.
#[test]
fn malformed_env_remove_keys_are_rejected_before_the_child_starts() {
    for (index, key) in ["", "KEY=value", "BAD KEY", "TAB\tKEY", "BEL\u{7}KEY"]
        .into_iter()
        .enumerate()
    {
        let dir = scratch(&format!("env-remove-invalid-{index}"));
        let out = run_with_flags(
            &dir,
            &[],
            &["--env-remove", key],
            shell_inline("echo child-must-not-start"),
        );
        assert_eq!(
            out.status.code(),
            Some(100),
            "malformed --env-remove value {key:?} must be a usage error; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stdout.is_empty(),
            "a rejected --env-remove value must not forward child output: {key:?}"
        );
        assert!(
            !events_path(&dir).exists(),
            "a rejected --env-remove value must fail before creating the event stream: {key:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A valid bare name still reaches ProcessKit's `env_remove` builder call and
/// removes the value inherited by the child.
#[test]
fn env_remove_removes_an_inherited_variable_through_the_binary() {
    const KEY: &str = "PROCESSKIT_TEST_ENV_REMOVE";
    const VALUE: &str = "inherited-before-remove";
    let program = if cfg!(windows) {
        format!("echo %{KEY}%")
    } else {
        format!("printf '%s\\n' \"${}\"", KEY)
    };
    let inherited = [(KEY, Path::new(VALUE))];

    let baseline_dir = scratch("env-remove-baseline");
    let baseline = run_with_flags(&baseline_dir, &inherited, &[], shell_inline(&program));
    assert_eq!(
        baseline.status.code(),
        Some(0),
        "the baseline child exits cleanly; stderr: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    assert!(
        String::from_utf8_lossy(&baseline.stdout).contains(VALUE),
        "the baseline proves the runner environment reaches the child: {:?}",
        String::from_utf8_lossy(&baseline.stdout)
    );

    let removed_dir = scratch("env-remove-applied");
    let removed = run_with_flags(
        &removed_dir,
        &inherited,
        &["--env-remove", KEY],
        shell_inline(&program),
    );
    assert_eq!(
        removed.status.code(),
        Some(0),
        "the child exits cleanly after removal; stderr: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&removed.stdout).contains(VALUE),
        "--env-remove strips the inherited value from the child: {:?}",
        String::from_utf8_lossy(&removed.stdout)
    );

    let _ = std::fs::remove_dir_all(&baseline_dir);
    let _ = std::fs::remove_dir_all(&removed_dir);
}

// ---------------------------------------------------------------------------
// Applied order, end to end (T-304, R-02).
//
// The three "Also given → Result" rows README.md and `docs/running-commands.md`
// publish — `--env-clear` → the key survives; `--env-remove KEY` → the key is
// still set; an `--env-file` entry for `KEY` → the run id wins — are claims about
// what the *child* observes, decided by where the injection sits in
// `src/run/launch.rs`'s builder chain. A `src/cli/run.rs` unit test cannot reach
// that: it sees which fields a command line populated, not which value survived to
// the child. These scenarios drive the built binary and read the value out of the
// child itself.
//
// Every scenario writes `--run-id-env` *before* the `--env-*` flag it is paired
// with, and the two order-sensitive ones run both orders, so "the outcome does not
// depend on argument order" is tested rather than declared. Each has a control run
// (K-059) proving the flag it is competing with really does something on its own —
// otherwise "the run id won" would also pass against a clear that cleared nothing
// or a file entry that never reached the child.
// ---------------------------------------------------------------------------

/// A child that prints what it observes for [`RUN_ID_ENV_KEY`], prefixed so the
/// value can be lifted out of the runner's live echo.
///
/// `cmd /c echo %VAR%` prints the literal `%VAR%` for an unset variable while `sh`
/// prints nothing; neither rendering can be mistaken for a run id or for one of the
/// sentinel values above, so no assertion here keys off how "unset" looks.
fn echo_run_id_env_program() -> Vec<String> {
    let script = if cfg!(windows) {
        format!("echo run-id-env:%{RUN_ID_ENV_KEY}%")
    } else {
        format!("echo \"run-id-env:${RUN_ID_ENV_KEY}\"")
    };
    shell_inline(&script)
}

/// Run one precedence scenario to completion and return the two values the
/// documented table is about: what the child observed for [`RUN_ID_ENV_KEY`], and
/// this run's own `run_started.run_id`.
///
/// `runner_envs` is set on the runner process and therefore inherited by the child —
/// that is how a scenario gives `--env-clear`/`--env-remove` something real to act
/// on. No `--run-id` is passed, so every scenario also runs against a *generated*
/// id: the value cannot have been read off the command line.
fn observe_run_id_env_with(
    tag: &str,
    runner_envs: &[(&str, &Path)],
    flags: &[&str],
) -> (String, String) {
    let dir = scratch(tag);
    let out = run_with_flags(&dir, runner_envs, flags, echo_run_id_env_program());
    assert_eq!(
        out.status.code(),
        Some(0),
        "the child exits cleanly for {flags:?}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let observed = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("run-id-env:").map(str::to_string))
        .unwrap_or_else(|| panic!("the child echoed the variable it read: {stdout:?}"));

    let events = read_run_events(&dir);
    let run_id = events
        .iter()
        .find(|event| event["event"] == "run_started")
        .unwrap_or_else(|| panic!("the run reported itself started: {events:?}"))["run_id"]
        .as_str()
        .expect("run_started names its run")
        .to_string();

    let _ = std::fs::remove_dir_all(&dir);
    (observed, run_id)
}

/// Row 1: with `--env-clear`, the injection is applied *after* the clear, so the key
/// is set on the emptied slate rather than wiped along with everything else.
///
/// What this scenario can and cannot catch, stated rather than implied: unlike the
/// two rows below, this one does not depend on where the injection sits among the
/// builder calls, because `processkit::Command::env_clear()` is an order-independent
/// flag (it decides whether the *inherited* environment is carried over) while
/// explicit sets are a separate ordered list. Moving the injection ahead of the
/// `env_clear()` call therefore does not change what the child observes — verified
/// by doing exactly that during this test's own self-check, where the two rows below
/// failed and this one did not. Its job is the user-facing promise itself: that this
/// key survives a cleared slate, which would break if the injection were dropped, or
/// if that library property ever changed.
#[test]
fn run_id_env_is_set_on_a_slate_env_clear_emptied() {
    let inherited: &[(&str, &Path)] = &[(RUN_ID_ENV_KEY, Path::new(RUN_ID_ENV_INHERITED))];

    // Control: the clear really empties the slate this key was on.
    let (cleared, _) =
        observe_run_id_env_with("run-id-env-clear-control", inherited, &["--env-clear"]);
    assert_ne!(
        cleared, RUN_ID_ENV_INHERITED,
        "control: --env-clear must actually wipe the inherited value, or the run below \
         would pass without the injection having done anything"
    );

    let (observed, run_id) = observe_run_id_env_with(
        "run-id-env-clear",
        inherited,
        &["--run-id-env", RUN_ID_ENV_KEY, "--env-clear"],
    );
    assert_eq!(
        observed, run_id,
        "the run id is injected after the clear, so the child observes it on the cleared slate"
    );
}

/// Row 2: an `--env-remove` for the very key being injected does not win — removals
/// are applied before every set, so the key is still set when the child starts.
#[test]
fn run_id_env_outlives_an_env_remove_for_the_same_key() {
    let inherited: &[(&str, &Path)] = &[(RUN_ID_ENV_KEY, Path::new(RUN_ID_ENV_INHERITED))];

    // Two controls: the value really is inherited by default, and the removal really
    // removes it. Without both, "the child observed the run id" could pass against a
    // removal that never happened.
    let (baseline, _) = observe_run_id_env_with("run-id-env-remove-baseline", inherited, &[]);
    assert_eq!(
        baseline, RUN_ID_ENV_INHERITED,
        "control: the runner's own environment reaches the child by default"
    );
    let (removed, _) = observe_run_id_env_with(
        "run-id-env-remove-control",
        inherited,
        &["--env-remove", RUN_ID_ENV_KEY],
    );
    assert_ne!(
        removed, RUN_ID_ENV_INHERITED,
        "control: --env-remove must actually strip the inherited value"
    );

    let orders: [&[&str]; 2] = [
        &[
            "--run-id-env",
            RUN_ID_ENV_KEY,
            "--env-remove",
            RUN_ID_ENV_KEY,
        ],
        &[
            "--env-remove",
            RUN_ID_ENV_KEY,
            "--run-id-env",
            RUN_ID_ENV_KEY,
        ],
    ];
    for flags in orders {
        let (observed, run_id) = observe_run_id_env_with("run-id-env-remove", inherited, flags);
        assert_eq!(
            observed, run_id,
            "the injection lands after the removal, whichever order the flags were \
             written: {flags:?}"
        );
    }
}

/// Row 3: an `--env-file` entry for the same key loses to the injection, which is
/// applied last — the case the parser cannot decide, since a file's contents are
/// read at run time rather than compared at parse time.
#[test]
fn run_id_env_wins_over_an_env_file_entry_for_the_same_key() {
    let dir = scratch("run-id-env-file");
    let env_file = dir.join("base.env");
    std::fs::write(
        &env_file,
        format!("{RUN_ID_ENV_KEY}={RUN_ID_ENV_FROM_FILE}\n"),
    )
    .expect("write the env-file fixture");
    let env_file = path_arg(&env_file);

    // Control: the file entry does reach the child on its own, so losing to the
    // injection below is a real precedence outcome and not an unread file.
    let (from_file, _) =
        observe_run_id_env_with("run-id-env-file-control", &[], &["--env-file", &env_file]);
    assert_eq!(
        from_file, RUN_ID_ENV_FROM_FILE,
        "control: the file's own value reaches the child without --run-id-env"
    );

    let orders: [&[&str]; 2] = [
        &["--run-id-env", RUN_ID_ENV_KEY, "--env-file", &env_file],
        &["--env-file", &env_file, "--run-id-env", RUN_ID_ENV_KEY],
    ];
    for flags in orders {
        let (observed, run_id) = observe_run_id_env_with("run-id-env-file-wins", &[], flags);
        assert_eq!(
            observed, run_id,
            "the injection is applied last, so the run id replaces the file's value \
             whichever order the flags were written: {flags:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The whole chain at once, with `--run-id-env` written first: a cleared slate, a
/// removal of the very key, and a file entry that sets it to something else still
/// end with the child observing this run's id. This is the applied-order claim
/// (`clear → remove → env-file → env → run-id injection`) as one statement, and the
/// end-to-end counterpart of the parse-level
/// `run_id_env_composes_with_every_environment_flag_in_any_order` unit test.
#[test]
fn run_id_env_wins_over_every_other_environment_flag_at_once() {
    let dir = scratch("run-id-env-chain");
    let env_file = dir.join("base.env");
    std::fs::write(
        &env_file,
        format!("{RUN_ID_ENV_KEY}={RUN_ID_ENV_FROM_FILE}\n"),
    )
    .expect("write the env-file fixture");
    let env_file = path_arg(&env_file);
    let inherited: &[(&str, &Path)] = &[(RUN_ID_ENV_KEY, Path::new(RUN_ID_ENV_INHERITED))];

    let (observed, run_id) = observe_run_id_env_with(
        "run-id-env-chain-run",
        inherited,
        &[
            "--run-id-env",
            RUN_ID_ENV_KEY,
            "--env-clear",
            "--env-remove",
            RUN_ID_ENV_KEY,
            "--env-file",
            &env_file,
            "--env",
            "PROCESSKIT_TEST_OTHER=untouched",
        ],
    );
    assert_eq!(
        observed, run_id,
        "the injection closes the chain, so nothing earlier in it decides this key"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A program that cannot be started is a runner-own failure, so the runner exits
/// with the reserved `SPAWN` code (101) and reports the reason on stderr — never
/// on stdout.
#[test]
fn missing_program_uses_the_spawn_code() {
    let dir = scratch("nofile");
    let out = run(&dir, &[], ["processkit_cli_no_such_program_xyz"]);
    assert_eq!(
        out.status.code(),
        Some(101),
        "a spawn failure exits with the reserved SPAWN code"
    );
    assert!(
        out.stdout.is_empty(),
        "a spawn failure writes nothing to the child's stdout"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("processkit-cli"),
        "the failure is reported on stderr: {stderr:?}"
    );
}

/// A `--max-memory` cap the platform cannot apply is a **fail-fast, pre-spawn**
/// failure: the run emits the resource-specific `limit_hit` event first, then the
/// shared `container_failed{create}` → `runner_exit{container_error, 102}` tail,
/// and never starts the child. Where the platform *can* apply the cap (a Windows
/// Job Object, or a Linux cgroup v2 at the real hierarchy root) the trivial child
/// simply runs — we honestly handle both outcomes rather than asserting an
/// enforcement we cannot reproduce across every CI host (`AGENTS.md`, testing
/// tiers; mirrors the `dotnet`-availability skip in the e2e tier). The end-to-end
/// wiring under test (the `limit_hit`+tail sequence, and its exit code) is proved
/// on every host that cannot apply the cap; the container-free `limit`-string
/// mapping is unit-tested cross-platform in `src/run/launch.rs`.
#[test]
fn resource_limit_that_cannot_be_applied_emits_limit_hit_and_the_backend_code() {
    let dir = scratch("limit");
    // A valid, ordinary cap: applied where a whole-tree container exists, and
    // fail-fast `limit_hit` where none does — never a silent no-op either way.
    let out = run_with_flags(&dir, &[], &["--max-memory", "64m"], shell_inline("exit 0"));
    let events = read_run_events(&dir);
    let position = |name: &str| events.iter().position(|event| event["event"] == name);

    match events.iter().find(|event| event["event"] == "limit_hit") {
        Some(limit_hit) => {
            // The platform/environment could not apply the cap.
            assert_eq!(
                limit_hit["limit"], "memory",
                "the emitted limit_hit names the memory cap: {limit_hit}"
            );
            assert!(
                !limit_hit["detail"].is_null(),
                "a human-readable detail accompanies the limit_hit: {limit_hit}"
            );
            // Pre-spawn: the child never started.
            assert!(
                position("run_started").is_none(),
                "a pre-spawn limit failure never starts the child: {events:?}"
            );
            assert!(
                position("limit_evidence").is_none(),
                "a pre-spawn limit failure has no ProcessGroup to query: {events:?}"
            );
            // The shared container-creation-failure tail follows, reusing BACKEND(102).
            let container_failed = events
                .iter()
                .find(|event| event["event"] == "container_failed")
                .expect("container_failed follows limit_hit on the create path");
            assert_eq!(container_failed["phase"], "create");
            assert_eq!(container_failed["code"], 102);
            let terminal = events.last().expect("a terminal runner_exit event");
            assert_eq!(terminal["event"], "runner_exit");
            assert_eq!(terminal["source"], "container_error");
            assert_eq!(terminal["code"], 102);
            assert!(terminal["child_code"].is_null());
            // Ordering: limit_hit before container_failed before runner_exit.
            assert!(
                position("limit_hit") < position("container_failed"),
                "limit_hit must precede container_failed: {events:?}"
            );
            assert!(
                position("container_failed") < position("runner_exit"),
                "container_failed must precede the terminal runner_exit: {events:?}"
            );
            // The process exit code and stderr agree with the stream.
            assert_eq!(
                out.status.code(),
                Some(102),
                "a cap that could not be applied exits with the reserved BACKEND code"
            );
            assert!(
                out.stdout.is_empty(),
                "no child output when the child never ran"
            );
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains("resource limit"),
                "the limit failure is named on stderr: {stderr:?}"
            );
        }
        None => {
            // The platform applied the cap: the trivial child ran to completion. We
            // deliberately do not assert the enforcement itself here (it is not
            // reproducible on every host); only that this is a normal child exit with
            // no limit failure and no BACKEND code.
            assert!(
                position("run_started").is_some(),
                "an applied cap still starts the child: {events:?}"
            );
            let terminal = events.last().expect("a terminal runner_exit event");
            assert_eq!(terminal["event"], "runner_exit");
            assert_eq!(
                terminal["source"], "child_exit",
                "an applied cap leaves a normal child exit: {events:?}"
            );
            assert_eq!(out.status.code(), Some(0), "the trivial child exits 0");
            let evidence = events
                .iter()
                .find(|event| event["event"] == "limit_evidence")
                .expect("an applied cap emits post-run evidence");
            assert_eq!(evidence["event"], "limit_evidence");
            assert!(
                position("root_exited") < position("limit_evidence"),
                "evidence follows the child outcome: {events:?}"
            );
            assert!(
                position("limit_evidence") < position("cleanup_started"),
                "evidence is read before teardown starts: {events:?}"
            );
            assert!(
                ["tripped", "not_tripped", "unknown"].contains(
                    &evidence["memory"]
                        .as_str()
                        .expect("memory verdict is a string")
                ),
                "the applied cap keeps the three-state contract: {evidence}"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A Linux cgroup-v2 process cap records a real fork refusal as `tripped`.
/// Hosts that cannot create a delegated cgroup take the documented pre-spawn
/// `limit_hit` path and are skipped rather than treating fallback as enforcement.
#[cfg(target_os = "linux")]
#[test]
fn linux_cgroup_process_limit_evidence_distinguishes_a_tripped_cap() {
    let dir = scratch("limit-tripped");
    let out = run_with_flags(
        &dir,
        &[],
        &["--max-processes", "1"],
        shell_inline("true & wait"),
    );
    let events = read_run_events(&dir);
    if events.iter().any(|event| event["event"] == "limit_hit") {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let evidence = events
        .iter()
        .find(|event| event["event"] == "limit_evidence")
        .expect("an applied Linux cap emits evidence");
    assert_eq!(evidence["processes"], "tripped");
    assert!(out.status.code().is_some(), "the runner returned a status");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A successful Windows Job Object exists, but ProcessKit cannot retain a
/// post-run limit counter for it. The CLI must preserve `Unknown` instead of
/// flattening the capped axis to `not_tripped`.
#[cfg(windows)]
#[test]
fn windows_job_limit_evidence_reports_unknown_for_the_capped_axis() {
    let dir = scratch("limit-windows-unknown");
    let out = run_with_flags(&dir, &[], &["--max-memory", "64m"], shell_inline("exit 0"));
    let events = read_run_events(&dir);
    assert!(
        events.iter().all(|event| event["event"] != "limit_hit"),
        "Windows Job Object creation should succeed for a valid memory cap: {events:?}"
    );
    assert_eq!(out.status.code(), Some(0), "the child exits normally");
    let evidence = events
        .iter()
        .find(|event| event["event"] == "limit_evidence")
        .expect("an applied Windows cap emits post-run evidence");
    assert_eq!(evidence["memory"], "unknown");
    assert_ne!(evidence["memory"], "not_tripped");
    let _ = std::fs::remove_dir_all(&dir);
}

/// macOS and the BSDs use a POSIX process group. ProcessKit rejects a capped
/// group during `with_options`, so the observable contract is the pre-spawn
/// `limit_hit` tail and no post-run evidence event.
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
#[test]
fn posix_process_group_limit_fallback_has_no_post_run_evidence() {
    let dir = scratch("limit-posix-fallback");
    let out = run_with_flags(&dir, &[], &["--max-memory", "64m"], shell_inline("exit 0"));
    let events = read_run_events(&dir);
    assert_pre_spawn_limit_failure_without_evidence(&events, &out);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Linux can use the process-group fallback when cgroup-v2 delegation is not
/// available. In that environment the fallback is the same pre-spawn contract;
/// when cgroup-v2 is available, the separate clean/tripped tests cover evidence.
#[cfg(target_os = "linux")]
#[test]
fn linux_process_group_fallback_has_no_post_run_evidence() {
    let dir = scratch("limit-linux-fallback");
    let out = run_with_flags(&dir, &[], &["--max-memory", "64m"], shell_inline("exit 0"));
    let events = read_run_events(&dir);
    if events.iter().any(|event| event["event"] == "limit_hit") {
        assert_pre_spawn_limit_failure_without_evidence(&events, &out);
    } else {
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "limit_evidence"),
            "a successfully created Linux cgroup has post-run evidence: {events:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A clean Linux cgroup-v2 run reports authoritative `not_tripped` evidence
/// separately from the process-count `tripped` scenario above. Hosts without
/// usable cgroup delegation take the pre-spawn fallback and are skipped.
#[cfg(target_os = "linux")]
#[test]
fn linux_cgroup_memory_limit_evidence_distinguishes_a_clean_cap() {
    let dir = scratch("limit-clean");
    let out = run_with_flags(&dir, &[], &["--max-memory", "64m"], shell_inline("exit 0"));
    let events = read_run_events(&dir);
    if events.iter().any(|event| event["event"] == "limit_hit") {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    assert_eq!(out.status.code(), Some(0), "the clean child exits normally");
    let evidence = events
        .iter()
        .find(|event| event["event"] == "limit_evidence")
        .expect("an applied Linux cap emits post-run evidence");
    assert_eq!(evidence["memory"], "not_tripped");
    assert_eq!(evidence["processes"], "not_tripped");
    assert_eq!(evidence["cpu"], "not_tripped");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every field `resource_summary` declares, for the reader assertions below.
const RESOURCE_SUMMARY_MEASUREMENTS: [&str; 5] = [
    "peak_memory_bytes",
    "total_cpu_ms",
    "io_read_bytes",
    "io_write_bytes",
    "peak_process_count",
];

/// The mandated ordering proof, **through the built binary** rather than against the
/// emitter in isolation: an ordinary successful run writes exactly one
/// `resource_summary`, after the ending is known, before teardown consumes the
/// container, and strictly before the terminal `runner_exit`.
///
/// The run requests **no cap and no flag** — that is the point. `limit_evidence` is
/// asserted absent in the same breath, so this pins that the summary is unconditional
/// rather than having quietly inherited its neighbour's `limits_requested` gate (which
/// is the shape a regression here would most plausibly take, since the two are emitted
/// from adjacent lines).
#[test]
fn a_normal_run_emits_one_resource_summary_before_the_terminal_event() {
    let dir = scratch("resource-summary-order");
    let out = run(&dir, &[], shell_inline("exit 0"));
    assert_eq!(out.status.code(), Some(0), "the trivial child exits 0");

    let events = read_run_events(&dir);
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|event| event["event"].as_str())
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "resource_summary").count(),
        1,
        "exactly one resource_summary per run: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"limit_evidence"),
        "this run requested no cap, so the summary is not riding limit_evidence's \
         condition: {kinds:?}"
    );

    let position = |tag: &str| {
        kinds
            .iter()
            .position(|found| *found == tag)
            .unwrap_or_else(|| panic!("the stream must contain a `{tag}` event: {kinds:?}"))
    };
    let summary_at = position("resource_summary");
    assert!(
        position("root_exited") < summary_at,
        "the summary is read after the ending is decided: {kinds:?}"
    );
    assert!(
        summary_at < position("cleanup_started"),
        "the summary is read before teardown consumes the container: {kinds:?}"
    );
    assert!(
        summary_at < position("runner_exit"),
        "the summary precedes the terminal event: {kinds:?}"
    );
    assert_eq!(
        kinds.last().copied(),
        Some("runner_exit"),
        "runner_exit is still the last line: {kinds:?}"
    );

    let summary = &events[summary_at];
    assert!(
        summary["read_error"].is_boolean(),
        "the qualifier is always present: {summary}"
    );
    for field in RESOURCE_SUMMARY_MEASUREMENTS {
        let value = summary
            .get(field)
            .unwrap_or_else(|| panic!("`{field}` is declared on every summary: {summary}"));
        assert!(
            value.is_null() || value.as_u64().is_some(),
            "`{field}` is either an honest null or a non-negative integer, never anything \
             else: {summary}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The platform-`null` half of the contract, asserted as the **exact** claim
/// `docs/resource-limits.md` makes rather than as a tolerance: on Windows a Job Object
/// keeps no peak-concurrency counter, so `peak_process_count` is `null` — always, on
/// every run, on every Windows host.
///
/// This is deliberately an equality, not an "is null or a number": a future change that
/// filled the field by taking a maximum over the runner's own `stats()` calls would
/// pass a permissive check while breaking the documented promise that this runner does
/// not manufacture a peak it never observed.
#[cfg(windows)]
#[test]
fn windows_resource_summary_reports_no_peak_process_count() {
    let dir = scratch("resource-summary-windows-null");
    let out = run(&dir, &[], shell_inline("exit 0"));
    assert_eq!(out.status.code(), Some(0), "the trivial child exits 0");

    let events = read_run_events(&dir);
    let summary = events
        .iter()
        .find(|event| event["event"] == "resource_summary")
        .expect("every run emits a resource_summary");
    assert_eq!(
        summary["read_error"], false,
        "a Windows Job Object can be read, so this is a confirmed reading: {summary}"
    );
    assert!(
        summary["peak_process_count"].is_null(),
        "a Job Object keeps no peak-concurrency counter, and the runner does not invent \
         one from its own reads: {summary}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The Linux counterpart, and the one axis whose absence is *conditional* rather than
/// absolute: the IO byte counters need the cgroup v2 `io` controller, which this CLI
/// never enables itself. So either the controller is there and the counters are real
/// numbers, or it is not and they are `null` — never a fabricated `0` standing in for
/// the missing controller.
///
/// Written as a two-way case analysis rather than a skip, because both outcomes are
/// documented behaviour and the host decides which one applies.
///
/// **Why there is no `> 0` lower bound here**, even though the child provably writes
/// 1 MiB: it would be unsound against this project's own documented contract, not
/// merely flaky. `io.stat`'s `wbytes` counts bytes that crossed the **block layer**,
/// and a write reaches it when the kernel writes the page back — which can happen after
/// the writing member exited, or not at all if the group is torn down while the page is
/// still dirty (`docs/resource-limits.md`, "What the tree consumed", consequence 4). A
/// short write-and-exit run legitimately reporting `0` is documented behaviour, so
/// asserting non-zero would encode a promise the runner deliberately does not make. The
/// `sync` in the workload makes a real number likely, and the assertions below pin what
/// is actually guaranteed: never a fabricated stand-in, and never one half of a counter
/// block without the other.
#[cfg(target_os = "linux")]
#[test]
fn linux_resource_summary_reports_io_bytes_only_where_the_controller_exists() {
    let dir = scratch("resource-summary-linux-io");
    // A child that actually writes, so a host with the `io` controller has real
    // traffic to report rather than a legitimately-zero counter.
    let out = run(
        &dir,
        &[],
        shell_inline("dd if=/dev/zero of=./io-probe.bin bs=64k count=16 2>/dev/null; sync"),
    );
    assert_eq!(out.status.code(), Some(0), "the writing child exits 0");

    let events = read_run_events(&dir);
    let summary = events
        .iter()
        .find(|event| event["event"] == "resource_summary")
        .expect("every run emits a resource_summary");

    for field in ["io_read_bytes", "io_write_bytes"] {
        let value = &summary[field];
        assert!(
            value.is_null() || value.as_u64().is_some(),
            "`{field}` is either absent (no `io` controller for this cgroup, or the \
             process-group fallback) or a real counter — never a stand-in: {summary}"
        );
    }
    // Whatever the controller situation, the two IO axes must agree about whether this
    // mechanism accounts for IO at all: they are read from the same counter block, so
    // one populated and the other null would mean the projection lost a value.
    assert_eq!(
        summary["io_read_bytes"].is_null(),
        summary["io_write_bytes"].is_null(),
        "both halves of one counter block are accounted for, or neither is: {summary}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The runner-imposed-ending path: a `--timeout` run must report consumption too, in
/// the same position relative to its own reason event as a natural exit does relative
/// to `root_exited`. A summary that only appeared on the happy path would leave the
/// endings an operator most wants to explain — the ones that were killed — silent about
/// what the tree was doing.
#[test]
fn a_timed_out_run_still_reports_what_the_tree_consumed() {
    let dir = scratch("resource-summary-timeout");
    let long_sleep = if cfg!(windows) {
        shell_inline("ping -n 300 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 300")
    };
    let out = run_with_flags(
        &dir,
        &[],
        &["--timeout", "1s", "--grace", "200ms"],
        long_sleep,
    );
    assert_eq!(
        out.status.code(),
        Some(106),
        "a timeout takes the reserved TIMEOUT code; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let events = read_run_events(&dir);
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|event| event["event"].as_str())
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "resource_summary").count(),
        1,
        "a runner-imposed ending emits exactly one summary too: {kinds:?}"
    );
    let position = |tag: &str| {
        kinds
            .iter()
            .position(|found| *found == tag)
            .unwrap_or_else(|| panic!("the stream must contain a `{tag}` event: {kinds:?}"))
    };
    assert!(
        position("timeout") < position("resource_summary")
            && position("resource_summary") < position("cleanup_started"),
        "the summary sits between the reason event and the teardown pair, exactly as it \
         sits between root_exited and the pair on a natural exit: {kinds:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The headline guarantee: after `run` returns, a descendant the child leaked and
/// abandoned does not survive. The child spawns a detached grandchild that
/// appends to a heartbeat file on a ~1s cadence, then the child exits. Once `run`
/// returns, the owning `ProcessGroup` has dropped and reaped the whole tree, so
/// the heartbeat stops: the file's size must not grow any further. This holds
/// regardless of teardown timing — a leaked grandchild would keep appending.
#[test]
fn tears_down_a_leaked_descendant() {
    let dir = scratch("teardown");
    let heartbeat = dir.join("heartbeat.txt");
    let grandchild = write_grandchild_script(&dir);
    let root = write_root_script(&dir, &grandchild);

    let program_and_args: Vec<String> = if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), path_arg(&root)]
    } else {
        vec!["/bin/sh".into(), path_arg(&root)]
    };

    let out = run(
        &dir,
        &[
            ("HB", heartbeat.as_path()),
            ("GRANDCHILD", grandchild.as_path()),
        ],
        program_and_args,
    );
    // The child (root) exits cleanly after launching the grandchild; the runner
    // forwards that 0.
    assert_eq!(out.status.code(), Some(0), "the root child exits cleanly");

    // By the time `run` returned the group had already been torn down, so the
    // grandchild is dead. It must have run at least once first (else the fixture
    // never launched it and the test would prove nothing).
    let size_at_return = file_len(&heartbeat);
    assert!(
        size_at_return > 0,
        "the grandchild must have started heartbeating before teardown"
    );

    // A leaked grandchild would append several more times in this window; a torn
    // down one cannot grow the file at all.
    sleep(Duration::from_secs(3));
    let size_later = file_len(&heartbeat);
    assert_eq!(
        size_later, size_at_return,
        "a leaked descendant kept heartbeating after run returned — teardown failed \
         (grew from {size_at_return} to {size_later} bytes)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Size of `path` in bytes, or 0 when it does not exist yet.
fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// `--capture-dir` records the child's stdout/stderr to `stdout.log`/`stderr.log`
/// **without** breaking the live echo, keeps the two streams separate, and — the
/// load-bearing property for this task (K-005) — still cannot hang when a leaked
/// descendant keeps an output handle open past the root's exit: the pump drain
/// stays time-bounded, so `run` returns promptly rather than blocking on the
/// grandchild's whole lifetime. The `output_captured` event reports each stream's
/// path, full byte counter, content hash, and an explicit (here `false`) truncation
/// flag.
#[test]
fn capture_records_streams_without_hanging_on_a_leaked_descendant() {
    let dir = scratch("capture");
    let heartbeat = dir.join("heartbeat.txt");
    let capture_dir = dir.join("capture");
    let grandchild = write_grandchild_script(&dir);
    let root = write_capture_root_script(&dir);

    let program_and_args: Vec<String> = if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), path_arg(&root)]
    } else {
        vec!["/bin/sh".into(), path_arg(&root)]
    };

    let capture_flag = path_arg(&capture_dir);
    let start = Instant::now();
    let out = run_with_flags(
        &dir,
        &[
            ("HB", heartbeat.as_path()),
            ("GRANDCHILD", grandchild.as_path()),
        ],
        &["--capture-dir", &capture_flag],
        program_and_args,
    );
    let elapsed = start.elapsed();

    // The root echoes and leaks the grandchild, then exits cleanly; the runner
    // forwards that 0 through the capture path.
    assert_eq!(out.status.code(), Some(0), "the root child exits cleanly");

    // No hang: the grandchild holds the child's stdout pipe and lives ~30s, but the
    // bounded pump drain lets `run` return in a small multiple of the ~5s teardown
    // window — nowhere near the grandchild's lifetime.
    assert!(
        elapsed < Duration::from_secs(25),
        "capture must not wait out the leaked descendant: run took {elapsed:?}"
    );

    // Live echo is preserved with capture on: the child's stdout still reaches the
    // runner's stdout, strictly separated from stderr.
    let live_stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        live_stdout.contains("CAPTURED_OUT"),
        "live echo of stdout must survive capture: {live_stdout:?}"
    );
    assert!(
        !live_stdout.contains("CAPTURED_ERR"),
        "child stderr must not bleed into the runner's stdout: {live_stdout:?}"
    );

    // The capture files hold the same output, separated per stream.
    let stdout_log =
        std::fs::read_to_string(capture_dir.join("stdout.log")).expect("stdout.log must exist");
    let stderr_log =
        std::fs::read_to_string(capture_dir.join("stderr.log")).expect("stderr.log must exist");
    assert!(
        stdout_log.contains("CAPTURED_OUT") && !stdout_log.contains("CAPTURED_ERR"),
        "stdout.log captures only stdout: {stdout_log:?}"
    );
    assert!(
        stderr_log.contains("CAPTURED_ERR") && !stderr_log.contains("CAPTURED_OUT"),
        "stderr.log captures only stderr: {stderr_log:?}"
    );

    // The `output_captured` event reports coherent per-stream metadata.
    let events = read_run_events(&dir);
    let captured = events
        .iter()
        .find(|e| e["event"] == "output_captured")
        .expect("an output_captured event when --capture-dir is set");
    let stdout_meta = &captured["stdout"];
    assert!(
        stdout_meta["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("stdout.log")),
        "the event names the stdout capture file: {captured}"
    );
    assert_eq!(
        stdout_meta["bytes"].as_u64(),
        Some(file_len(&capture_dir.join("stdout.log"))),
        "an untruncated stream's byte counter equals its file size"
    );
    assert!(
        is_sha256_hex(&stdout_meta["sha256"]),
        "the stdout capture carries a hex content hash: {captured}"
    );
    assert_eq!(
        stdout_meta["truncated"], false,
        "a small stream is captured in full, not truncated: {captured}"
    );
    assert_eq!(captured["stderr"]["truncated"], false);

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--capture-max-bytes <size>` (T-181) overrides the default 8 MiB per-stream
/// ceiling: a small custom ceiling actually clips the captured file at exactly
/// that value — not the default — while the full byte counter and the explicit
/// `truncated` flag still report the whole (unclipped) truth, exactly as they do
/// at the default ceiling.
#[test]
fn custom_capture_max_bytes_clips_the_stream_at_the_configured_ceiling() {
    let dir = scratch("capture-max-bytes");
    let capture_dir = dir.join("capture");
    let ceiling: u64 = 50;
    let script = write_overflow_stdout_script(&dir);
    let program_and_args: Vec<String> = if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), path_arg(&script)]
    } else {
        vec!["/bin/sh".into(), path_arg(&script)]
    };

    let capture_flag = path_arg(&capture_dir);
    let out = run_with_flags(
        &dir,
        &[],
        &[
            "--capture-dir",
            &capture_flag,
            "--capture-max-bytes",
            &ceiling.to_string(),
        ],
        program_and_args,
    );
    assert_eq!(out.status.code(), Some(0), "the child exits cleanly");

    // The file on disk holds exactly the *configured* ceiling's worth, not the
    // default 8 MiB one.
    let on_disk_len = file_len(&capture_dir.join("stdout.log"));
    assert_eq!(
        on_disk_len, ceiling,
        "the capture file must be clipped at the configured ceiling"
    );

    let events = read_run_events(&dir);
    let captured = events
        .iter()
        .find(|e| e["event"] == "output_captured")
        .expect("an output_captured event when --capture-dir is set");
    let stdout_meta = &captured["stdout"];
    assert_eq!(
        stdout_meta["truncated"], true,
        "the stream outran the configured ceiling: {captured}"
    );
    assert!(
        stdout_meta["bytes"].as_u64().is_some_and(|b| b > ceiling),
        "the full byte counter must exceed the configured ceiling: {captured}"
    );
}

/// The opt-in overflow policy turns a noisy, non-idle child into a distinct
/// runner-owned ending. It must use the shared graceful teardown, preserve the
/// bounded capture metadata, and never alias a child exit or a time deadline.
#[test]
fn capture_overflow_cancel_ends_a_run_with_a_distinct_outcome() {
    let dir = scratch("capture-overflow-cancel");
    let capture_dir = dir.join("capture");
    let program_and_args = shell_inline(if cfg!(windows) {
        "for /L %i in (1,1,1000000) do @echo 0123456789abcdef"
    } else {
        "while :; do printf '0123456789abcdef\\n'; done"
    });
    let capture_flag = path_arg(&capture_dir);
    let out = run_with_flags(
        &dir,
        &[],
        &[
            "--capture-dir",
            &capture_flag,
            "--capture-max-bytes",
            "64",
            "--capture-overflow",
            "cancel",
            "--no-echo",
            "--grace",
            "10ms",
        ],
        program_and_args,
    );

    assert_eq!(
        out.status.code(),
        Some(113),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events = read_run_events(&dir);
    let kinds: Vec<_> = events
        .iter()
        .filter_map(|event| event["event"].as_str())
        .collect();
    let overflow = events
        .iter()
        .find(|event| event["event"] == "output_overflow")
        .expect("an output_overflow event");
    assert_eq!(overflow["stream"], "stdout");
    assert_eq!(overflow["max_bytes"], 64);
    assert_eq!(overflow["grace_ms"], 10);
    // The shared graceful teardown, with the unconditional `resource_summary` read
    // between the reason event and the pair — this run requested no cap, so no
    // `limit_evidence` sits alongside it, and the window is contiguous.
    assert!(
        kinds.windows(4).any(|window| window
            == [
                "output_overflow",
                "resource_summary",
                "cleanup_started",
                "cleanup_finished"
            ]),
        "overflow must enter the shared graceful teardown: {kinds:?}"
    );
    let captured = events
        .iter()
        .find(|event| event["event"] == "output_captured")
        .expect("forced endings still report capture metadata");
    assert_eq!(captured["stdout"]["truncated"], true);
    assert_eq!(file_len(&capture_dir.join("stdout.log")), 64);
    let terminal = events.last().expect("terminal runner_exit");
    assert_eq!(terminal["event"], "runner_exit");
    assert_eq!(terminal["source"], "output_overflow");
    assert_eq!(terminal["code"], 113);
    assert!(terminal["child_code"].is_null());

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--no-echo` (T-196) suppresses only the runner's live retransmission of the
/// child's stdout/stderr on the runner's own stdout/stderr — the pipe, the pump,
/// `--capture-dir`, and the JSONL event stream are all otherwise unaffected.
/// Proven by running the exact same inline script twice — once plain, once with
/// `--no-echo` added — and comparing: the plain run's own stdout/stderr carry the
/// child's output as usual (see `passes_child_streams_through_without_mixing`);
/// the `--no-echo` run's carry none of it; both runs' `--capture-dir` files hold
/// the exact same bytes; and both runs' JSONL streams carry the same sequence of
/// event types and agree on the fields `--no-echo` is actually contracted not to
/// change (`root_exited`, `runner_exit`, and `output_captured`'s per-stream
/// byte/hash/truncation/write-error metadata — see `assert_run_outcome_matches`/
/// `assert_output_captured_matches`). This deliberately stops short of a
/// full-payload comparison: some fields (e.g. `cleanup_finished.remaining`,
/// `members_snapshot.members`) are live, racy snapshots of process-tree state
/// that this project documents as such, not deterministic values two separate
/// invocations are guaranteed to agree on (see `Event::CleanupFinished`'s doc
/// comment) — comparing them would make this test flaky rather than prove
/// anything about `--no-echo`.
#[test]
fn no_echo_suppresses_the_live_relay_but_leaves_capture_and_events_whole() {
    let script = if cfg!(windows) {
        "echo NO_ECHO_OUT&echo NO_ECHO_ERR 1>&2"
    } else {
        "echo NO_ECHO_OUT; echo NO_ECHO_ERR 1>&2"
    };

    // Baseline: the same script and a --capture-dir, without --no-echo.
    let baseline_dir = scratch("no-echo-baseline");
    let baseline_capture = baseline_dir.join("capture");
    let baseline_capture_flag = path_arg(&baseline_capture);
    let baseline_out = run_with_flags(
        &baseline_dir,
        &[],
        &[
            "--capture-dir",
            &baseline_capture_flag,
            "--run-id",
            "no-echo-fixture",
        ],
        shell_inline(script),
    );
    assert_eq!(
        baseline_out.status.code(),
        Some(0),
        "baseline child exits cleanly"
    );
    let baseline_stdout = String::from_utf8_lossy(&baseline_out.stdout).into_owned();
    let baseline_stderr = String::from_utf8_lossy(&baseline_out.stderr).into_owned();
    assert!(
        baseline_stdout.contains("NO_ECHO_OUT"),
        "baseline echoes the child's stdout: {baseline_stdout:?}"
    );
    assert!(
        baseline_stderr.contains("NO_ECHO_ERR"),
        "baseline echoes the child's stderr: {baseline_stderr:?}"
    );

    // The flag under test: same script and capture shape, plus --no-echo.
    let no_echo_dir = scratch("no-echo-flag");
    let no_echo_capture = no_echo_dir.join("capture");
    let no_echo_capture_flag = path_arg(&no_echo_capture);
    let no_echo_out = run_with_flags(
        &no_echo_dir,
        &[],
        &[
            "--capture-dir",
            &no_echo_capture_flag,
            "--run-id",
            "no-echo-fixture",
            "--no-echo",
        ],
        shell_inline(script),
    );
    assert_eq!(
        no_echo_out.status.code(),
        Some(0),
        "--no-echo child exits cleanly"
    );
    let no_echo_stdout = String::from_utf8_lossy(&no_echo_out.stdout).into_owned();
    let no_echo_stderr = String::from_utf8_lossy(&no_echo_out.stderr).into_owned();
    assert!(
        !no_echo_stdout.contains("NO_ECHO_OUT") && !no_echo_stdout.contains("NO_ECHO_ERR"),
        "--no-echo must suppress the live stdout relay: {no_echo_stdout:?}"
    );
    assert!(
        !no_echo_stderr.contains("NO_ECHO_OUT") && !no_echo_stderr.contains("NO_ECHO_ERR"),
        "--no-echo must suppress the live stderr relay: {no_echo_stderr:?}"
    );

    // `--capture-dir` still receives every byte, identical to the baseline run.
    let baseline_stdout_log =
        std::fs::read(baseline_capture.join("stdout.log")).expect("baseline stdout.log exists");
    let baseline_stderr_log =
        std::fs::read(baseline_capture.join("stderr.log")).expect("baseline stderr.log exists");
    let no_echo_stdout_log =
        std::fs::read(no_echo_capture.join("stdout.log")).expect("--no-echo stdout.log exists");
    let no_echo_stderr_log =
        std::fs::read(no_echo_capture.join("stderr.log")).expect("--no-echo stderr.log exists");
    assert_eq!(
        baseline_stdout_log, no_echo_stdout_log,
        "--no-echo must not change what --capture-dir records on stdout"
    );
    assert_eq!(
        baseline_stderr_log, no_echo_stderr_log,
        "--no-echo must not change what --capture-dir records on stderr"
    );
    assert!(
        String::from_utf8_lossy(&no_echo_stdout_log).contains("NO_ECHO_OUT"),
        "the capture file is full even with the live echo suppressed"
    );

    // The JSONL stream's *shape* is unaffected by --no-echo: same sequence of
    // event types, and the same values on the handful of fields the flag is
    // actually contracted not to change. This deliberately does **not** compare
    // full payloads: several fields are live, racy snapshots of process-tree
    // state at the moment they were read (`cleanup_started.members_before`,
    // `cleanup_finished.remaining`, `members_snapshot.members`) and are
    // documented as such (see `Event::CleanupFinished`'s doc comment) — they can
    // legitimately differ between two otherwise-identical invocations of the
    // same script on a loaded host or a different teardown mechanism, and
    // comparing them here would make this test flaky rather than prove anything
    // about `--no-echo`.
    let baseline_events = read_run_events(&baseline_dir);
    let no_echo_events = read_run_events(&no_echo_dir);
    assert_eq!(
        event_type_sequence(&baseline_events),
        event_type_sequence(&no_echo_events),
        "--no-echo must not change the sequence of emitted event types"
    );
    assert_run_outcome_matches(&baseline_events, &no_echo_events);
    assert_output_captured_matches(&baseline_events, &no_echo_events);

    let _ = std::fs::remove_dir_all(&baseline_dir);
    let _ = std::fs::remove_dir_all(&no_echo_dir);
}

/// The ordered sequence of `event` tag values in a run's JSONL stream (e.g.
/// `["run_started", "members_snapshot", "root_exited", ...]`) — the part of the
/// stream that is deterministic across two otherwise-identical invocations of
/// the same script, unlike the live process-tree snapshots some events carry.
fn event_type_sequence(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|e| e["event"].as_str().expect("every event has a tag"))
        .collect()
}

/// Asserts that two runs' `root_exited` and `runner_exit` events agree on
/// exactly the fields `--no-echo` is contracted not to change: the child's own
/// outcome (`outcome`/`code`/`signal`) and the runner's terminal verdict
/// (`code`/`source`/`child_code`). Both events are expected in each stream —
/// this is a natural child exit, not a runner-imposed ending.
fn assert_run_outcome_matches(baseline: &[Value], no_echo: &[Value]) {
    let find = |events: &[Value], tag: &str| -> Value {
        events
            .iter()
            .find(|e| e["event"] == tag)
            .unwrap_or_else(|| panic!("a {tag} event is present"))
            .clone()
    };

    let baseline_root_exited = find(baseline, "root_exited");
    let no_echo_root_exited = find(no_echo, "root_exited");
    for field in ["outcome", "code", "signal"] {
        assert_eq!(
            baseline_root_exited[field], no_echo_root_exited[field],
            "--no-echo must not change root_exited.{field}"
        );
    }

    let baseline_runner_exit = find(baseline, "runner_exit");
    let no_echo_runner_exit = find(no_echo, "runner_exit");
    for field in ["code", "source", "child_code"] {
        assert_eq!(
            baseline_runner_exit[field], no_echo_runner_exit[field],
            "--no-echo must not change runner_exit.{field}"
        );
    }
}

/// Asserts that two runs' `output_captured` events agree on exactly the fields
/// `--capture-dir` is contracted to report regardless of `--no-echo`: the full
/// byte counter, content hash, truncation flag, and write-error flag, for both
/// streams. `path` is deliberately excluded — it embeds the per-run scratch
/// directory and is expected to differ.
fn assert_output_captured_matches(baseline: &[Value], no_echo: &[Value]) {
    let baseline_captured = baseline
        .iter()
        .find(|e| e["event"] == "output_captured")
        .expect("baseline run emits output_captured");
    let no_echo_captured = no_echo
        .iter()
        .find(|e| e["event"] == "output_captured")
        .expect("--no-echo run emits output_captured");
    for stream in ["stdout", "stderr"] {
        for field in ["bytes", "sha256", "truncated", "write_error"] {
            assert_eq!(
                baseline_captured[stream][field], no_echo_captured[stream][field],
                "--no-echo must not change output_captured.{stream}.{field}"
            );
        }
    }
}

/// `--no-echo` with **no** `--capture-dir` — the main documented use case
/// (README/CHANGELOG advertise it for an embedding orchestrator that reads the
/// result from `--jsonl` alone): the runner's own stdout/stderr carry none of
/// the child's output, while `root_exited`/`runner_exit` still report the
/// child's real outcome unchanged. The child actually writes marker bytes on
/// both streams (a silent `exit 0` child can't distinguish a working
/// suppression from a relay that was never wired up in the first place — see
/// R-03), and a baseline run of the same script *without* `--no-echo` proves
/// those markers do reach the runner's streams absent the flag.
#[test]
fn bare_no_echo_suppresses_the_relay_without_capture_dir() {
    let script = if cfg!(windows) {
        "echo NO_ECHO_BARE_OUT&echo NO_ECHO_BARE_ERR 1>&2"
    } else {
        "echo NO_ECHO_BARE_OUT; echo NO_ECHO_BARE_ERR 1>&2"
    };

    // Baseline: same script, no --no-echo — the markers must reach the
    // runner's own stdout/stderr, or the flagged run below would prove
    // nothing.
    let baseline_dir = scratch("no-echo-bare-baseline");
    let baseline_out = run_with_flags(&baseline_dir, &[], &[], shell_inline(script));
    assert_eq!(
        baseline_out.status.code(),
        Some(0),
        "baseline child exits cleanly"
    );
    let baseline_stdout = String::from_utf8_lossy(&baseline_out.stdout).into_owned();
    let baseline_stderr = String::from_utf8_lossy(&baseline_out.stderr).into_owned();
    assert!(
        baseline_stdout.contains("NO_ECHO_BARE_OUT"),
        "baseline (no --no-echo) echoes the child's stdout: {baseline_stdout:?}"
    );
    assert!(
        baseline_stderr.contains("NO_ECHO_BARE_ERR"),
        "baseline (no --no-echo) echoes the child's stderr: {baseline_stderr:?}"
    );

    let dir = scratch("no-echo-bare");
    let out = run_with_flags(&dir, &[], &["--no-echo"], shell_inline(script));
    assert_eq!(out.status.code(), Some(0), "the child exits cleanly");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !stdout.contains("NO_ECHO_BARE_OUT") && !stdout.contains("NO_ECHO_BARE_ERR"),
        "--no-echo without --capture-dir must still suppress the runner's stdout relay: {stdout:?}"
    );
    assert!(
        !stderr.contains("NO_ECHO_BARE_OUT") && !stderr.contains("NO_ECHO_BARE_ERR"),
        "--no-echo without --capture-dir must still suppress the runner's stderr relay: {stderr:?}"
    );

    let events = read_run_events(&dir);
    let root_exited = events
        .iter()
        .find(|e| e["event"] == "root_exited")
        .expect("a root_exited event");
    assert_eq!(root_exited["outcome"], "exited");
    assert_eq!(root_exited["code"], 0);

    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(runner_exit["event"], "runner_exit");
    assert_eq!(runner_exit["code"], 0);
    assert_eq!(runner_exit["source"], "child_exit");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&baseline_dir);
}

/// `--no-echo` combined with `--idle-timeout` (K-050): the `IdleClock` must stay
/// wired to the pump *underneath* the suppressed echo, not just the
/// `--capture-dir` tee — a silent child is still reaped on the idle deadline,
/// and a child that keeps producing output (even though none of it reaches the
/// runner's own stdout/stderr) still resets the idle clock and is left to exit
/// on its own. Mirrors `idle_timeout_emits_timeout_with_idle_reason`
/// (`tests/events.rs`) and the chatty-vs-silent pairing in
/// `idle_timeout_reaps_a_silent_child_but_spares_a_chatty_one` (`tests/e2e.rs`),
/// with `--no-echo` added to both sides.
#[test]
fn no_echo_still_lets_idle_timeout_reap_a_silent_child_but_spares_a_chatty_one() {
    // Silent child: with --no-echo, the runner's own stdout must stay empty
    // (the child never writes to it anyway) *and* the idle deadline must still
    // fire — proving the idle clock observes the pump, not the (now-discarded)
    // echo sink.
    let silent_dir = scratch("no-echo-idle-silent");
    let long_silent = if cfg!(windows) {
        shell_inline("ping -n 300 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 300")
    };
    let silent_out = run_with_flags(
        &silent_dir,
        &[],
        &["--idle-timeout", "1s", "--grace", "500ms", "--no-echo"],
        long_silent,
    );
    assert_eq!(
        silent_out.status.code(),
        Some(106),
        "a silent child under --no-echo is still reaped by --idle-timeout with the reserved \
         TIMEOUT code; stderr: {}",
        String::from_utf8_lossy(&silent_out.stderr)
    );
    // Only the *child's* echo is suppressed by --no-echo; the runner's own
    // idle-timeout diagnostic (a bounded, runner-authored line, distinct from
    // anything the silent child could have written) is unaffected and expected
    // on stderr here, exactly as it is without --no-echo.
    assert!(
        silent_out.stdout.is_empty(),
        "--no-echo suppresses the child's stdout relay even on an idle-timeout ending: {:?}",
        String::from_utf8_lossy(&silent_out.stdout)
    );
    let silent_events = read_run_events(&silent_dir);
    let timeout = silent_events
        .iter()
        .find(|e| e["event"] == "timeout")
        .expect("the silent child produced a timeout event");
    assert_eq!(
        timeout["reason"], "idle",
        "the --idle-timeout trigger is reported with reason=idle: {timeout}"
    );
    let _ = std::fs::remove_dir_all(&silent_dir);

    // Chatty child: keeps writing well inside the idle window, so it must
    // outlive the run and exit on its own even though --no-echo discards every
    // byte it writes before it ever reaches the runner's own stdout/stderr.
    let chatty_dir = scratch("no-echo-idle-chatty");
    let chatty_script = if cfg!(windows) {
        "for /L %i in (1,1,3) do (echo tick & ping -n 2 127.0.0.1 >nul)"
    } else {
        "for i in 1 2 3; do echo tick; sleep 1; done"
    };
    let chatty_out = run_with_flags(
        &chatty_dir,
        &[],
        &["--idle-timeout", "2s", "--no-echo"],
        shell_inline(chatty_script),
    );
    assert_eq!(
        chatty_out.status.code(),
        Some(0),
        "a child whose gaps stay under the idle window outlives --idle-timeout even under \
         --no-echo; stderr: {}",
        String::from_utf8_lossy(&chatty_out.stderr)
    );
    assert!(
        chatty_out.stdout.is_empty() && chatty_out.stderr.is_empty(),
        "--no-echo suppresses the relay for the chatty child too"
    );
    let chatty_events = read_run_events(&chatty_dir);
    assert!(
        chatty_events.iter().all(|e| e["event"] != "timeout"),
        "a child whose silences stay under the idle window must not trigger an idle timeout \
         even under --no-echo: {chatty_events:?}"
    );
    let _ = std::fs::remove_dir_all(&chatty_dir);
}

// ---------------------------------------------------------------------------
// `--snapshot-interval` (T-298): the opt-in periodic `members_snapshot` cadence.
// ---------------------------------------------------------------------------

/// How long the snapshot-cadence fixtures' child occupies the container. Short
/// enough to keep the suite quick, long enough that a sub-second cadence has room
/// for several re-samples inside it.
const SNAPSHOT_CHILD_SECONDS: u64 = 2;

/// A child that simply holds the container for `seconds` while producing no output
/// — the "long and quiet" run this feature exists for, and the case where nothing
/// but a snapshot could observe the tree. On Windows `ping -n N` waits N-1 seconds
/// between echo requests, so it is asked for one more.
fn quiet_child_for(seconds: u64) -> Vec<String> {
    if cfg!(windows) {
        shell_inline(&format!("ping -n {} 127.0.0.1 >nul", seconds + 1))
    } else {
        shell_inline(&format!("sleep {seconds}"))
    }
}

/// The `reason` of every `members_snapshot` in a stream, in emission order — the
/// one field that tells the post-spawn snapshot (`spawn`) from a
/// `--snapshot-interval` re-sample (`interval`).
fn snapshot_reasons(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter(|event| event["event"] == "members_snapshot")
        .map(|event| {
            event["reason"]
                .as_str()
                .unwrap_or_else(|| panic!("every members_snapshot carries a reason: {event}"))
        })
        .collect()
}

/// How many `reason: "interval"` snapshots a stream carries.
fn interval_snapshot_count(events: &[Value]) -> usize {
    snapshot_reasons(events)
        .into_iter()
        .filter(|reason| *reason == "interval")
        .count()
}

/// `--snapshot-interval` re-emits `members_snapshot` on roughly the requested
/// cadence for a long-lived child, and the *value* — not merely the flag's
/// presence — is what paces it: over the same ~2s child a 300ms cadence yields
/// several re-samples while a 5s cadence yields none at all (which also proves the
/// first re-sample waits a full interval instead of firing immediately).
///
/// The bounds are deliberately loose in both directions. The lower bound proves the
/// cadence repeats; the upper bound proves it is *paced* rather than a busy loop
/// (which would emit thousands per second, not a handful) without turning a slow
/// CI runner into a failing feature (K-058).
#[test]
fn snapshot_interval_re_emits_members_snapshot_on_the_requested_cadence() {
    let fast_dir = scratch("snapshot-interval-fast");
    let fast_out = run_with_flags(
        &fast_dir,
        &[],
        &["--snapshot-interval", "300ms"],
        quiet_child_for(SNAPSHOT_CHILD_SECONDS),
    );
    assert_eq!(
        fast_out.status.code(),
        Some(0),
        "the cadence must not disturb the child's own exit; stderr: {}",
        String::from_utf8_lossy(&fast_out.stderr)
    );
    let fast_events = read_run_events(&fast_dir);
    let fast_reasons = snapshot_reasons(&fast_events);
    assert_eq!(
        fast_reasons.first().copied(),
        Some("spawn"),
        "the post-spawn snapshot still comes first, and still says so: {fast_reasons:?}"
    );
    assert!(
        fast_reasons[1..].iter().all(|reason| *reason == "interval"),
        "exactly one snapshot is the post-spawn one; every later one is a re-sample: \
         {fast_reasons:?}"
    );
    let fast_count = interval_snapshot_count(&fast_events);
    assert!(
        (3..=60).contains(&fast_count),
        "a ~{SNAPSHOT_CHILD_SECONDS}s child at a 300ms cadence must produce several \
         re-samples, and a bounded number of them (a busy poll would produce orders of \
         magnitude more); got {fast_count}"
    );

    // A periodic snapshot is a real `members_info()` read through the shared
    // enrichment path, not an empty stub: at least one re-sample must actually list
    // a member with a PID (the child is alive throughout the cadence).
    assert!(
        fast_events
            .iter()
            .filter(|event| event["event"] == "members_snapshot" && event["reason"] == "interval")
            .any(|event| event["members"]
                .as_array()
                .is_some_and(|members| members.iter().any(|m| m["pid"].is_number()))),
        "a periodic snapshot must carry the container's real members: {fast_events:?}"
    );

    // Ordering (`docs/schema.md`, "Ordering"): every snapshot lands before the
    // ending's own event, and none is interleaved into the teardown pair.
    let types = event_type_sequence(&fast_events);
    let last_snapshot = types
        .iter()
        .rposition(|event| *event == "members_snapshot")
        .expect("the stream carries snapshots");
    let root_exited = types
        .iter()
        .position(|event| *event == "root_exited")
        .expect("a natural exit reports root_exited");
    assert!(
        last_snapshot < root_exited,
        "the cadence must stop when the ending is decided, never reach the teardown \
         tail: {types:?}"
    );
    let _ = std::fs::remove_dir_all(&fast_dir);

    // Same child, a cadence longer than its whole life: the interval value paces the
    // stream, and the first re-sample is not emitted at t=0.
    let slow_dir = scratch("snapshot-interval-slow");
    let slow_out = run_with_flags(
        &slow_dir,
        &[],
        &["--snapshot-interval", "5s"],
        quiet_child_for(SNAPSHOT_CHILD_SECONDS),
    );
    assert_eq!(
        slow_out.status.code(),
        Some(0),
        "the child still exits on its own under a long cadence; stderr: {}",
        String::from_utf8_lossy(&slow_out.stderr)
    );
    let slow_events = read_run_events(&slow_dir);
    let slow_count = interval_snapshot_count(&slow_events);
    assert!(
        slow_count <= 1,
        "a 5s cadence must not fire repeatedly inside a ~{SNAPSHOT_CHILD_SECONDS}s child \
         (and must not fire immediately on arming): got {slow_count}"
    );
    assert!(
        fast_count > slow_count,
        "the requested interval, not merely the flag's presence, must pace the cadence: \
         300ms gave {fast_count} re-samples, 5s gave {slow_count}"
    );
    let _ = std::fs::remove_dir_all(&slow_dir);
}

/// **Differential proof that omitting the flag changes nothing** (K-059: prove an
/// absence by comparison, never by an assertion that would pass vacuously). The
/// identical child is run twice — once bare, once with `--snapshot-interval` — and
/// the bare run is shown to still emit *exactly one* `members_snapshot`, while the
/// flagged run emits strictly more. Collapsing the flagged run's snapshot repeats
/// then reproduces the bare run's event sequence exactly, so the cadence is proven
/// to add snapshots and nothing else: no event gained, lost, or reordered.
#[test]
fn a_run_without_snapshot_interval_still_emits_exactly_one_members_snapshot() {
    let baseline_dir = scratch("snapshot-interval-baseline");
    let baseline_out = run_with_flags(
        &baseline_dir,
        &[],
        &[],
        quiet_child_for(SNAPSHOT_CHILD_SECONDS),
    );
    assert_eq!(
        baseline_out.status.code(),
        Some(0),
        "baseline child exits 0"
    );
    let baseline_events = read_run_events(&baseline_dir);
    assert_eq!(
        snapshot_reasons(&baseline_events),
        vec!["spawn"],
        "without the flag a run emits exactly one members_snapshot, the post-spawn one"
    );

    let cadence_dir = scratch("snapshot-interval-cadence");
    let cadence_out = run_with_flags(
        &cadence_dir,
        &[],
        &["--snapshot-interval", "300ms"],
        quiet_child_for(SNAPSHOT_CHILD_SECONDS),
    );
    assert_eq!(cadence_out.status.code(), Some(0), "cadence child exits 0");
    let cadence_events = read_run_events(&cadence_dir);
    let cadence_count = interval_snapshot_count(&cadence_events);
    assert!(
        cadence_count >= 1,
        "the comparison is only meaningful if the flagged run really did re-sample; \
         got {cadence_count}"
    );

    // Collapse consecutive `members_snapshot` lines: what remains must be the
    // baseline's stream, event for event.
    let mut collapsed: Vec<&str> = Vec::new();
    for event in event_type_sequence(&cadence_events) {
        if event == "members_snapshot" && collapsed.last() == Some(&"members_snapshot") {
            continue;
        }
        collapsed.push(event);
    }
    assert_eq!(
        collapsed,
        event_type_sequence(&baseline_events),
        "the cadence must add members_snapshot events and change nothing else"
    );

    let _ = std::fs::remove_dir_all(&baseline_dir);
    let _ = std::fs::remove_dir_all(&cadence_dir);
}

/// The cadence composes with `--inherit-stdio` — proved by running it, not asserted
/// in prose. Under direct inheritance the runner runs no output pump at all (which
/// is exactly why `--idle-timeout` conflicts with the flag), so this is the case
/// that shows a snapshot is a query of the *container's* member list rather than an
/// observation of the child's output.
#[test]
fn snapshot_interval_composes_with_inherit_stdio() {
    let dir = scratch("snapshot-interval-inherit-stdio");
    let out = run_with_flags(
        &dir,
        &[],
        &["--inherit-stdio", "--snapshot-interval", "300ms"],
        quiet_child_for(SNAPSHOT_CHILD_SECONDS),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "--inherit-stdio + --snapshot-interval is a valid combination that runs to \
         completion; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events = read_run_events(&dir);
    let reasons = snapshot_reasons(&events);
    assert_eq!(
        reasons.first().copied(),
        Some("spawn"),
        "the post-spawn snapshot is unaffected by the I/O mode: {reasons:?}"
    );
    assert!(
        interval_snapshot_count(&events) >= 2,
        "the cadence must keep sampling with no output pump in the run at all: {reasons:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// `--detach` (T-198)
//
// Every test below pins its own registry directory (`PROCESSKIT_CLI_REGISTRY_DIR`),
// because a detached run outlives the call that started it: without the override it
// would publish into the developer's real per-user registry and could collide with
// a concurrently running test's `run_id`.
// ---------------------------------------------------------------------------

/// How long the detach fixtures' child works before finishing. Long enough that a
/// call returning "immediately" is unmistakably distinguishable from one that waited
/// for the child, short enough to keep the suite quick. The margin is deliberately
/// generous — a detached start is two process spawns, and a loaded CI runner (or a
/// Windows host scanning each new executable) can make those cost real time; a tighter
/// window would turn a slow machine into a failing feature (K-058).
const DETACH_CHILD_WORK: Duration = Duration::from_secs(5);

/// How long a detach test will wait for something the *detached* run must do on its
/// own (finish its child, close its stream). Generous: it bounds a failure, never a
/// healthy path.
const DETACH_OBSERVE_TIMEOUT: Duration = Duration::from_secs(45);

/// The registry-directory override for one detach scenario, as the `envs` pair the
/// fixtures take. The detached copy inherits it from the call that spawns it, so the
/// whole chain — caller, detached runner, and the `list`/`inspect`/`cancel` clients
/// below — agrees on one throwaway registry.
fn detach_registry(dir: &Path) -> PathBuf {
    dir.join("registry")
}

/// Invoke a non-`run` subcommand of the binary against `registry` and wait for it.
fn cli_against(registry: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .expect("spawn the runner binary")
}

/// Parse the events written **so far** by a run that may still be going, skipping a
/// trailing line that is still being written. `read_run_events` deliberately panics
/// on a malformed line (a finished stream must be well-formed); a live stream is the
/// one case where an incomplete last line is normal rather than a contract violation.
fn read_events_so_far(dir: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(events_path(dir)).unwrap_or_default();
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

/// Whether the run's stream has been closed by its terminal `runner_exit`.
fn run_is_over(dir: &Path) -> bool {
    read_events_so_far(dir)
        .last()
        .is_some_and(|event| event["event"] == "runner_exit")
}

/// A child that works for [`DETACH_CHILD_WORK`] and only then creates the file named
/// by the `MARKER` environment variable. The marker is what makes "the call returned
/// before the child finished" an observation rather than a stopwatch reading: at the
/// moment a detached call returns, the file must not exist yet.
fn write_marker_after_work_script(dir: &Path) -> PathBuf {
    let seconds = DETACH_CHILD_WORK.as_secs();
    if cfg!(windows) {
        let path = dir.join("marker_after_work.bat");
        // `ping -n N` waits N-1 seconds between echo requests.
        let body = format!(
            "@echo off\r\n\
             ping -n {} 127.0.0.1 >nul\r\n\
             echo done>\"%MARKER%\"\r\n",
            seconds + 1
        );
        std::fs::write(&path, body).expect("write marker_after_work.bat");
        path
    } else {
        let path = dir.join("marker_after_work.sh");
        let body = format!("#!/bin/sh\nsleep {seconds}\nprintf done > \"$MARKER\"\n");
        std::fs::write(&path, body).expect("write marker_after_work.sh");
        path
    }
}

/// The platform invocation for one of the fixture scripts this file writes (the
/// detach marker script above, and the `--run-id-env` observer near the top).
fn script_program(path: &Path) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), path_arg(path)]
    } else {
        vec!["/bin/sh".into(), path_arg(path)]
    }
}

/// The headline contract of `--detach`: the call returns once the run has *started*,
/// not once the child has *finished* — and it returns `0` for the start, whatever the
/// child later does.
///
/// Proven differentially rather than by a stopwatch alone (K-059): the same script,
/// which creates a marker file only after working for [`DETACH_CHILD_WORK`], is run
/// twice. Without `--detach` the call returns only after the marker exists (that is
/// what "foreground" means, and it is what makes the flagged run below meaningful);
/// with `--detach` the call returns while the marker still does not exist, the run's
/// stream already carries `run_started` but no terminal `runner_exit`, and both the
/// marker and that terminal event appear afterwards — the run kept going without its
/// caller.
#[test]
fn detach_returns_once_the_run_has_started_while_a_foreground_run_waits_for_the_child() {
    // Baseline: no --detach. The call must outlast the child's work.
    let baseline_dir = scratch("detach-baseline");
    let baseline_registry = detach_registry(&baseline_dir);
    let baseline_marker = baseline_dir.join("finished.marker");
    let baseline_script = write_marker_after_work_script(&baseline_dir);
    let baseline_started = Instant::now();
    let baseline = run_with_flags(
        &baseline_dir,
        &[
            ("PROCESSKIT_CLI_REGISTRY_DIR", baseline_registry.as_path()),
            ("MARKER", baseline_marker.as_path()),
        ],
        &["--run-id", "detach-baseline"],
        script_program(&baseline_script),
    );
    let baseline_elapsed = baseline_started.elapsed();
    assert_eq!(
        baseline.status.code(),
        Some(0),
        "the baseline child exits cleanly; stderr: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    assert!(
        baseline_marker.exists(),
        "a foreground run returns only after its child is done — without this the \
         detached comparison below would prove nothing"
    );
    assert!(
        baseline_elapsed >= DETACH_CHILD_WORK,
        "a foreground run waits out the child's work ({baseline_elapsed:?})"
    );

    // The flag under test: same script, same shape, plus --detach.
    let dir = scratch("detach-fast");
    let registry = detach_registry(&dir);
    let marker = dir.join("finished.marker");
    let script = write_marker_after_work_script(&dir);
    let started = Instant::now();
    let out = run_with_flags(
        &dir,
        &[
            ("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path()),
            ("MARKER", marker.as_path()),
        ],
        &["--run-id", "detach-fast", "--detach"],
        script_program(&script),
    );
    let elapsed = started.elapsed();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a started detached run exits 0 for the start; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !marker.exists(),
        "--detach must return before the child finishes its work"
    );
    assert!(
        elapsed < baseline_elapsed,
        "--detach returned in {elapsed:?}, no sooner than the foreground run's \
         {baseline_elapsed:?}"
    );

    // The handshake's promise, checked at the moment of return: the run has provably
    // started (its `run_started` is already durable in --jsonl) and has provably not
    // ended (no terminal event yet).
    let at_return = read_events_so_far(&dir);
    assert!(
        at_return
            .iter()
            .any(|e| e["event"] == "run_started" && e["run_id"] == "detach-fast"),
        "--detach returns only after the run's own run_started is readable: {at_return:?}"
    );
    assert!(
        at_return.iter().all(|e| e["event"] != "runner_exit"),
        "the run must still be live when --detach returns: {at_return:?}"
    );

    // And it finishes on its own, with the child's real outcome recorded where a
    // detached caller can read it.
    wait_until(|| marker.exists(), DETACH_OBSERVE_TIMEOUT);
    wait_until(|| run_is_over(&dir), DETACH_OBSERVE_TIMEOUT);
    let events = read_run_events(&dir);
    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(runner_exit["event"], "runner_exit");
    assert_eq!(
        runner_exit["source"], "child_exit",
        "the detached run ended on its child's own exit: {runner_exit}"
    );
    assert_eq!(
        runner_exit["child_code"], 0,
        "the child's real exit code lives in runner_exit, not in the detached call's \
         own exit code: {runner_exit}"
    );

    let _ = std::fs::remove_dir_all(&baseline_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A detached run is an ordinary run to everything that supervises runs: it is
/// registered (so `list` finds it and `inspect` reaches it over the control plane)
/// and it is steerable (so `cancel` ends it), and its stream closes with the terminal
/// `runner_exit` naming that ending. This is the whole point of returning only after
/// the registry record exists — the caller can hand the `run_id` straight to the
/// supervision commands without polling for the run to appear.
#[test]
fn a_detached_run_is_discoverable_inspectable_and_cancellable() {
    let dir = scratch("detach-supervised");
    let registry = detach_registry(&dir);
    // A child that would run far longer than this test: whatever ends it, it is not
    // the child finishing on its own.
    let long_lived = if cfg!(windows) {
        shell_inline("ping -n 300 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 300")
    };
    let out = run_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "detach-supervised", "--detach"],
        long_lived,
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "the detached run started; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The ordering this test depends on, asserted rather than assumed: the run had
    // already reported itself started when the call returned, which is why the
    // supervision commands below can use the `run_id` straight away instead of
    // polling for the run to show up.
    assert!(
        read_events_so_far(&dir)
            .iter()
            .any(|e| e["event"] == "run_started" && e["run_id"] == "detach-supervised"),
        "--detach returns only after the run has started, so supervision needs no \
         warm-up poll"
    );

    // Discovery: the entry is already there, and live, the moment the call returns.
    let listed = cli_against(&registry, &["list", "--json"]);
    assert_eq!(listed.status.code(), Some(0), "list succeeds");
    let listed_out = String::from_utf8_lossy(&listed.stdout).into_owned();
    let entry: Value = listed_out
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|entry| entry["run_id"] == "detach-supervised")
        .unwrap_or_else(|| panic!("the detached run is registered: {listed_out:?}"));
    assert_eq!(
        entry["health"], "live",
        "the detached run is live, not a stale leftover: {entry}"
    );

    // Control plane: `inspect` reaches the detached runner itself.
    let inspected = cli_against(
        &registry,
        &["inspect", "--run-id", "detach-supervised", "--json"],
    );
    assert_eq!(
        inspected.status.code(),
        Some(0),
        "inspect reaches the detached runner; stderr: {}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    let snapshot: Value = serde_json::from_slice(&inspected.stdout).expect("inspect prints JSON");
    assert_eq!(snapshot["run_id"], "detach-supervised");
    assert!(
        snapshot["root_pid"].as_u64().is_some(),
        "the snapshot names the detached run's root child: {snapshot}"
    );

    // The run is still live at this point — so the cancel below is what ends it,
    // rather than the test observing a child that had already exited.
    assert!(
        !run_is_over(&dir),
        "the detached run must still be live before it is cancelled: {:?}",
        read_events_so_far(&dir)
    );

    // Steering: `cancel` ends the detached run through the ordinary teardown.
    let cancelled = cli_against(&registry, &["cancel", "--run-id", "detach-supervised"]);
    assert_eq!(
        cancelled.status.code(),
        Some(0),
        "cancel is accepted; stderr: {}",
        String::from_utf8_lossy(&cancelled.stderr)
    );

    wait_until(|| run_is_over(&dir), DETACH_OBSERVE_TIMEOUT);
    let events = read_run_events(&dir);
    let cancel_event = events
        .iter()
        .find(|e| e["event"] == "cancelled")
        .unwrap_or_else(|| panic!("the cancel is recorded in the run's stream: {events:?}"));
    assert_eq!(cancel_event["source"], "control_cancel");
    let runner_exit = events.last().expect("a terminal event");
    assert_eq!(runner_exit["event"], "runner_exit");
    assert_eq!(runner_exit["source"], "control_cancel");
    assert_eq!(
        runner_exit["code"], 108,
        "a detached run ends with the same reserved code a foreground one would: \
         {runner_exit}"
    );

    // The entry goes with the run: a cancelled detached run leaves no registry
    // leftovers behind for the next caller to trip over.
    let listed_after = cli_against(&registry, &["list", "--json"]);
    let listed_after_out = String::from_utf8_lossy(&listed_after.stdout).into_owned();
    assert!(
        !listed_after_out.contains("detach-supervised"),
        "the cancelled detached run removed its own registry entry: {listed_after_out:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A detached run relays none of the child's output to the caller — there is nobody
/// left to relay it to — while everything that *records* the output keeps working.
///
/// Differential, not an absence-only assertion (K-059): the same marker-writing child
/// runs twice. Without `--detach` both markers reach the call's own stdout/stderr,
/// which is what proves they would otherwise be relayed; with `--detach` neither
/// does, and the `--capture-dir` transcript of the detached run holds those very
/// bytes — so the output was produced and observed, just not echoed at the caller.
#[test]
fn detach_relays_no_child_output_that_a_foreground_run_would_echo() {
    let script = if cfg!(windows) {
        "echo DETACH_OUT&echo DETACH_ERR 1>&2"
    } else {
        "echo DETACH_OUT; echo DETACH_ERR 1>&2"
    };

    // Baseline: same script, no --detach — the markers must reach the call's own
    // streams, or the flagged run below would prove nothing.
    let baseline_dir = scratch("detach-echo-baseline");
    let baseline_registry = detach_registry(&baseline_dir);
    let baseline = run_with_flags(
        &baseline_dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", baseline_registry.as_path())],
        &["--run-id", "detach-echo-baseline"],
        shell_inline(script),
    );
    assert_eq!(
        baseline.status.code(),
        Some(0),
        "the baseline child exits cleanly"
    );
    let baseline_stdout = String::from_utf8_lossy(&baseline.stdout).into_owned();
    let baseline_stderr = String::from_utf8_lossy(&baseline.stderr).into_owned();
    assert!(
        baseline_stdout.contains("DETACH_OUT"),
        "the foreground run echoes the child's stdout: {baseline_stdout:?}"
    );
    assert!(
        baseline_stderr.contains("DETACH_ERR"),
        "the foreground run echoes the child's stderr: {baseline_stderr:?}"
    );

    // The flag under test, with a transcript so the bytes are still provably observed.
    let dir = scratch("detach-echo");
    let registry = detach_registry(&dir);
    let capture = dir.join("capture");
    let capture_flag = path_arg(&capture);
    let out = run_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &[
            "--run-id",
            "detach-echo",
            "--detach",
            "--capture-dir",
            &capture_flag,
        ],
        shell_inline(script),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "the detached run started; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !stdout.contains("DETACH_OUT") && !stdout.contains("DETACH_ERR"),
        "a detached run relays nothing to the caller's stdout: {stdout:?}"
    );
    assert!(
        !stderr.contains("DETACH_OUT") && !stderr.contains("DETACH_ERR"),
        "a detached run relays nothing to the caller's stderr: {stderr:?}"
    );

    // ...but the run did observe every byte: the transcript holds both markers.
    wait_until(|| run_is_over(&dir), DETACH_OBSERVE_TIMEOUT);
    let captured_stdout =
        std::fs::read_to_string(capture.join("stdout.log")).expect("the transcript exists");
    let captured_stderr =
        std::fs::read_to_string(capture.join("stderr.log")).expect("the transcript exists");
    assert!(
        captured_stdout.contains("DETACH_OUT"),
        "the detached run captured the child's stdout: {captured_stdout:?}"
    );
    assert!(
        captured_stderr.contains("DETACH_ERR"),
        "the detached run captured the child's stderr: {captured_stderr:?}"
    );

    let _ = std::fs::remove_dir_all(&baseline_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A start that fails is never reported as a started run, and the code the caller
/// sees is the same one the failure produces in the foreground — `--detach` mints no
/// code of its own (K-047).
///
/// Differential on the code itself: each failure is run twice, once with `--detach`
/// and once without, and the two exit codes must match. A missing program fails
/// inside the detached copy (which reports it in the run's own stream before dying);
/// an uncreatable `--jsonl` fails in the caller, before anything is spawned.
#[test]
fn a_detached_start_failure_reports_the_same_code_the_foreground_would() {
    // The child program does not exist: `SPAWN` (101), from the detached copy.
    let dir = scratch("detach-spawn-failure");
    let registry = detach_registry(&dir);
    let missing = ["definitely-not-a-real-program-t198"];
    let foreground = run_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "detach-spawn-foreground"],
        missing,
    );
    assert_eq!(
        foreground.status.code(),
        Some(101),
        "a missing program is a SPAWN failure in the foreground; stderr: {}",
        String::from_utf8_lossy(&foreground.stderr)
    );

    let detached_dir = scratch("detach-spawn-failure-detached");
    let detached_registry = detach_registry(&detached_dir);
    let detached = run_with_flags(
        &detached_dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", detached_registry.as_path())],
        &["--run-id", "detach-spawn-detached", "--detach"],
        missing,
    );
    assert_eq!(
        detached.status.code(),
        foreground.status.code(),
        "a failed detached start reports the very code the foreground run reports, \
         never a silent success; stderr: {}",
        String::from_utf8_lossy(&detached.stderr)
    );
    let detached_stderr = String::from_utf8_lossy(&detached.stderr).into_owned();
    assert!(
        detached_stderr.contains("did not start"),
        "the failure says the run never started: {detached_stderr:?}"
    );
    // The detached copy's own account of the failure survives in the run's stream.
    let events = read_run_events(&detached_dir);
    assert!(
        events.iter().any(|e| e["event"] == "spawn_failed"),
        "the detached copy recorded why it could not start: {events:?}"
    );
    let terminal = events.last().expect("a terminal event");
    assert_eq!(terminal["event"], "runner_exit");
    assert_eq!(terminal["source"], "spawn_error");

    // An events file that cannot be created: `SETUP` (111), from the caller itself —
    // there is nowhere for a detached copy to report it, so it is never spawned.
    let jsonl_dir = scratch("detach-setup-failure");
    let unwritable = jsonl_dir.join("missing-parent").join("events.jsonl");
    let unwritable_flag = path_arg(&unwritable);
    let trivial = shell_inline("exit 0");
    let mut foreground_setup = Command::new(bin());
    foreground_setup
        .arg("run")
        .arg("--jsonl")
        .arg(&unwritable_flag)
        .arg("--")
        .args(&trivial);
    let foreground_setup = foreground_setup.output().expect("spawn the runner binary");
    assert_eq!(
        foreground_setup.status.code(),
        Some(111),
        "an uncreatable --jsonl is a SETUP failure in the foreground; stderr: {}",
        String::from_utf8_lossy(&foreground_setup.stderr)
    );

    let mut detached_setup = Command::new(bin());
    detached_setup
        .arg("run")
        .arg("--jsonl")
        .arg(&unwritable_flag)
        .arg("--detach")
        .arg("--")
        .args(&trivial);
    let detached_setup = detached_setup.output().expect("spawn the runner binary");
    assert_eq!(
        detached_setup.status.code(),
        foreground_setup.status.code(),
        "--detach reports an uncreatable --jsonl exactly as the foreground does; \
         stderr: {}",
        String::from_utf8_lossy(&detached_setup.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&detached_dir);
    let _ = std::fs::remove_dir_all(&jsonl_dir);
}

/// A script that writes well over 50 bytes to stdout (repeated fixed-width
/// lines), so any small `--capture-max-bytes` ceiling well below that is
/// guaranteed to clip it. The exact per-platform byte count is not load-bearing
/// (line endings differ), only that it comfortably exceeds the small ceiling the
/// test configures.
fn write_overflow_stdout_script(dir: &Path) -> std::path::PathBuf {
    if cfg!(windows) {
        let path = dir.join("overflow_stdout.bat");
        let body = "@echo off\r\n\
             for /L %%i in (1,1,50) do echo 0123456789ABCDEF\r\n";
        std::fs::write(&path, body).expect("write overflow_stdout.bat");
        path
    } else {
        let path = dir.join("overflow_stdout.sh");
        let body = "#!/bin/sh\n\
             i=0\n\
             while [ \"$i\" -lt 50 ]; do\n\
             \x20 printf '0123456789ABCDEF\\n'\n\
             \x20 i=$((i + 1))\n\
             done\n";
        std::fs::write(&path, body).expect("write overflow_stdout.sh");
        path
    }
}

/// Assert the complete pre-spawn contract for a cap request that ProcessKit
/// cannot install. There is no group to query, so `limit_hit` is the only
/// resource-specific event and the existing backend tail remains unchanged.
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn assert_pre_spawn_limit_failure_without_evidence(events: &[Value], out: &Output) {
    let position = |name: &str| events.iter().position(|event| event["event"] == name);
    let limit_hit = events
        .iter()
        .find(|event| event["event"] == "limit_hit")
        .expect("the pre-spawn fallback emits limit_hit");
    assert_eq!(limit_hit["limit"], "memory");
    assert!(
        !limit_hit["detail"].is_null(),
        "the limit failure retains its detail: {limit_hit}"
    );
    assert!(position("run_started").is_none());
    assert!(position("limit_evidence").is_none());
    assert_eq!(out.status.code(), Some(102));
    let container_failed = events
        .iter()
        .find(|event| event["event"] == "container_failed")
        .expect("container_failed follows limit_hit");
    assert_eq!(container_failed["phase"], "create");
    assert_eq!(container_failed["code"], 102);
    let terminal = events.last().expect("the stream has a terminal event");
    assert_eq!(terminal["event"], "runner_exit");
    assert_eq!(terminal["source"], "container_error");
    assert_eq!(terminal["code"], 102);
    assert!(position("limit_hit") < position("container_failed"));
    assert!(position("container_failed") < position("runner_exit"));
}

/// Parse the emitted JSONL event stream for `dir`, one object per non-empty line.
fn read_run_events(dir: &Path) -> Vec<Value> {
    let path = events_path(dir);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read events file {}: {err}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("each event line is valid JSON"))
        .collect()
}

/// A natural input-consuming run remains a normal child exit: cleanup completes
/// and the terminal event retains the child's code rather than minting a runner code.
fn assert_child_exit_event(dir: &Path) {
    let events = read_run_events(dir);
    let cleanup = events
        .iter()
        .find(|event| event["event"] == "cleanup_finished")
        .unwrap_or_else(|| panic!("interactive input does not bypass cleanup: {events:?}"));
    assert_eq!(cleanup["remaining"], 0, "cleanup must leave no members");
    assert_eq!(
        cleanup["remaining_pids"],
        serde_json::json!([]),
        "cleanup must leave no member PIDs"
    );
    let terminal = events.last().expect("a terminal event");
    assert_eq!(terminal["event"], "runner_exit");
    assert_eq!(terminal["source"], "child_exit");
    assert_eq!(terminal["code"], 0);
    assert_eq!(terminal["child_code"], 0);
}

/// A batch file avoids cmd.exe's single-line variable-expansion rules while the
/// POSIX script uses the same one-line input contract.
fn stdin_reader_program(dir: &Path) -> Vec<String> {
    if cfg!(windows) {
        let script = dir.join("read-stdin.bat");
        std::fs::write(
            &script,
            "@echo off\r\nset /p line=\r\necho stdin:%line%\r\n",
        )
        .expect("write Windows stdin reader");
        vec!["cmd".into(), "/c".into(), path_arg(&script)]
    } else {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "IFS= read -r line; printf 'stdin:%s\\n' \"$line\"".into(),
        ]
    }
}

/// Read one line, then write it to both output streams. A script file avoids
/// cmd.exe's single-command variable-expansion rules on Windows.
fn stdio_reader_program(dir: &Path) -> Vec<String> {
    if cfg!(windows) {
        let script = dir.join("read-stdio.bat");
        std::fs::write(
            &script,
            "@echo off\r\nset /p line=\r\necho stdio-out:%line%\r\necho stdio-err:%line% 1>&2\r\n",
        )
        .expect("write Windows stdio reader");
        vec!["cmd".into(), "/c".into(), path_arg(&script)]
    } else {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "IFS= read -r line; printf 'stdio-out:%s\\n' \"$line\"; printf 'stdio-err:%s\\n' \"$line\" >&2".into(),
        ]
    }
}

/// Whether `v` is a JSON string of 64 lowercase-hex characters (a SHA-256 digest).
fn is_sha256_hex(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Write a root script that echoes a marker to stdout *and* stderr, launches the
/// detached heartbeat grandchild (which keeps the inherited stdout handle open past
/// the root's exit — the leaked-descendant shape), and exits. Used to prove capture
/// records both streams without hanging on the survivor.
fn write_capture_root_script(dir: &Path) -> std::path::PathBuf {
    if cfg!(windows) {
        let path = dir.join("capture_root.bat");
        let body = "@echo off\r\n\
             echo CAPTURED_OUT\r\n\
             echo CAPTURED_ERR 1>&2\r\n\
             start \"\" /b \"%GRANDCHILD%\"\r\n";
        std::fs::write(&path, body).expect("write capture_root.bat");
        path
    } else {
        let path = dir.join("capture_root.sh");
        let body = "#!/bin/sh\n\
             echo CAPTURED_OUT\n\
             echo CAPTURED_ERR 1>&2\n\
             sh \"$GRANDCHILD\" &\n\
             exit 0\n";
        std::fs::write(&path, body).expect("write capture_root.sh");
        path
    }
}

/// A program argument as a lossless platform string (paths are never re-parsed by
/// a shell here, so lossy UTF-8 is fine for the temp paths the fixture builds).
fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Write the grandchild script: a bounded heartbeat loop (append, wait ~1s) so a
/// leaked instance keeps growing the file while a reaped one stops. Bounded to
/// ~30 iterations so a teardown regression self-terminates instead of running
/// forever.
fn write_grandchild_script(dir: &Path) -> std::path::PathBuf {
    if cfg!(windows) {
        let path = dir.join("grandchild.bat");
        // CRLF and the `do ( … )` block shape are what cmd's batch parser expects.
        let body = "@echo off\r\n\
             for /L %%i in (1,1,30) do (\r\n\
             \x20 echo x>>\"%HB%\"\r\n\
             \x20 ping -n 2 127.0.0.1 >nul\r\n\
             )\r\n";
        std::fs::write(&path, body).expect("write grandchild.bat");
        path
    } else {
        let path = dir.join("grandchild.sh");
        let body = "#!/bin/sh\n\
             i=0\n\
             while [ \"$i\" -lt 30 ]; do\n\
             \x20 printf x >> \"$HB\"\n\
             \x20 sleep 1\n\
             \x20 i=$((i + 1))\n\
             done\n";
        std::fs::write(&path, body).expect("write grandchild.sh");
        path
    }
}

/// Write the root script: launch the grandchild detached (so it outlives the
/// root) and exit immediately, leaving a live descendant behind. The grandchild
/// deliberately keeps the inherited stdout handle, which is exactly the "leaked
/// descendant holds the pipe" shape teardown must still resolve.
fn write_root_script(dir: &Path, grandchild: &Path) -> std::path::PathBuf {
    let _ = grandchild; // path travels via the GRANDCHILD env var, not argv.
    if cfg!(windows) {
        let path = dir.join("root.bat");
        let body = "@echo off\r\nstart \"\" /b \"%GRANDCHILD%\"\r\n";
        std::fs::write(&path, body).expect("write root.bat");
        path
    } else {
        let path = dir.join("root.sh");
        let body = "#!/bin/sh\nsh \"$GRANDCHILD\" &\nexit 0\n";
        std::fs::write(&path, body).expect("write root.sh");
        path
    }
}

/// Write a root script that launches the detached heartbeat grandchild and then
/// *stays alive* (a long sleep), so a runner-imposed ending (a `--timeout` or a
/// `Ctrl-C`) is what stops it — the shape the teardown-on-timeout/cancel proofs
/// need, in contrast to [`write_root_script`]'s immediately-exiting root.
fn write_sleeping_root_script(dir: &Path) -> std::path::PathBuf {
    if cfg!(windows) {
        let path = dir.join("sleeping_root.bat");
        let body = "@echo off\r\n\
             start \"\" /b \"%GRANDCHILD%\"\r\n\
             ping -n 300 127.0.0.1 >nul\r\n";
        std::fs::write(&path, body).expect("write sleeping_root.bat");
        path
    } else {
        let path = dir.join("sleeping_root.sh");
        let body = "#!/bin/sh\nsh \"$GRANDCHILD\" &\nsleep 300\n";
        std::fs::write(&path, body).expect("write sleeping_root.sh");
        path
    }
}

/// A `--timeout` that elapses is a **distinguishable, runner-imposed** ending: the
/// runner exits with the reserved `TIMEOUT` code (106, never the child's own),
/// explains it on stderr, and — the headline guarantee — tears the whole tree
/// down. The child sleeps long past the deadline while a detached grandchild
/// heartbeats; once the runner returns the heartbeat must stop.
#[test]
fn timeout_reports_the_timeout_code_and_tears_down_the_tree() {
    let dir = scratch("timeout");
    let heartbeat = dir.join("heartbeat.txt");
    let grandchild = write_grandchild_script(&dir);
    let root = write_sleeping_root_script(&dir);

    let program_and_args: Vec<String> = if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), path_arg(&root)]
    } else {
        vec!["/bin/sh".into(), path_arg(&root)]
    };

    let out = run_with_flags(
        &dir,
        &[
            ("HB", heartbeat.as_path()),
            ("GRANDCHILD", grandchild.as_path()),
        ],
        &["--timeout", "2s"],
        program_and_args,
    );

    // A runner-imposed timeout takes the reserved code, not a forwarded child code.
    assert_eq!(
        out.status.code(),
        Some(106),
        "a timeout must exit with the reserved TIMEOUT code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("timed out"),
        "the timeout must be explained on stderr: {stderr:?}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("processkit-cli"),
        "no runner diagnostic may appear on the child's stdout"
    );

    // The grandchild must have heartbeat before teardown (else the fixture proved
    // nothing) and must be gone now: a torn-down tree cannot grow the file.
    let size_at_return = file_len(&heartbeat);
    assert!(
        size_at_return > 0,
        "the grandchild must have started heartbeating before the timeout"
    );
    sleep(Duration::from_secs(3));
    let size_later = file_len(&heartbeat);
    assert_eq!(
        size_later, size_at_return,
        "a descendant survived the timeout teardown (grew from {size_at_return} to {size_later})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Windows honesty: a Job Object has no POSIX signal, and ProcessKit's Windows soft
/// tier (a `WM_CLOSE` to a windowed member, or a `CTRL_BREAK` to an opted-in console
/// leader) can reach nothing at all in a plain console child's tree — the ordinary
/// case, and exactly what this child is. Such a timeout must *say so plainly*: it
/// names the atomic Job Object kill, states that nothing in the tree could receive a
/// soft close, and never claims a graceful soft-terminate was performed
/// (`docs/ROADMAP.md`: "a Windows cancellation that reached nothing must keep
/// reporting its hard-kill fallback honestly").
#[cfg(windows)]
#[test]
fn windows_timeout_reports_the_hard_kill_fallback_honestly() {
    let dir = scratch("wintimeout");
    let out = run_with_flags(
        &dir,
        &[],
        &["--timeout", "1s", "--grace", "1s"],
        ["cmd", "/c", "ping -n 300 127.0.0.1 >nul"],
    );
    assert_eq!(
        out.status.code(),
        Some(106),
        "a timeout exits with the TIMEOUT code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Windows"),
        "the degradation is named: {stderr:?}"
    );
    assert!(
        stderr.contains("Job Object"),
        "the atomic kill is named: {stderr:?}"
    );
    assert!(
        stderr.contains("no soft-terminate"),
        "honesty: no soft stop was delivered: {stderr:?}"
    );
    assert!(
        stderr.contains("no windowed member") && stderr.contains("no console-CTRL leader"),
        "honesty: the reason nothing was delivered is stated, not just the outcome: {stderr:?}"
    );
    assert!(
        !stderr.contains("sent SIGTERM"),
        "must not claim a soft signal was delivered on Windows: {stderr:?}"
    );
    assert!(
        !stderr.contains("WM_CLOSE"),
        "a console child owns no window, so nothing was closed either: {stderr:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Unix soft path: where a real soft-terminate exists, the timeout message states
/// the `SIGTERM` was sent and the grace was waited — the honest counterpart to the
/// Windows fallback above.
#[cfg(unix)]
#[test]
fn unix_timeout_reports_a_real_soft_signal() {
    let dir = scratch("unixtimeout");
    let out = run_with_flags(
        &dir,
        &[],
        &["--timeout", "1s", "--grace", "1s"],
        shell_inline("sleep 300"),
    );
    assert_eq!(
        out.status.code(),
        Some(106),
        "a timeout exits with the TIMEOUT code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("SIGTERM"),
        "the real soft signal is named: {stderr:?}"
    );
    assert!(
        stderr.contains("grace"),
        "the grace window is named: {stderr:?}"
    );
    assert!(
        !stderr.contains("Windows"),
        "the Unix message must not mention the Windows fallback: {stderr:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--grace` really holds a pause between the soft signal and the hard kill (Unix,
/// where the soft path exists). The child *ignores* `SIGTERM`, so the runner must
/// wait the full grace before the kill-on-drop `SIGKILL`: the run cannot end until
/// roughly `timeout + grace`, well past the deadline alone.
#[cfg(unix)]
#[test]
fn grace_holds_the_pause_before_the_hard_kill() {
    let dir = scratch("grace");
    let start = std::time::Instant::now();
    // Trap (ignore) SIGTERM in the shell; the busy `sleep 1` loop re-arms after the
    // one-shot broadcast kills its in-flight sleep, so the tree outlives the soft
    // signal and only dies at the post-grace SIGKILL.
    let out = run_with_flags(
        &dir,
        &[],
        &["--timeout", "1s", "--grace", "3s"],
        shell_inline("trap '' TERM; while :; do sleep 1; done"),
    );
    let elapsed = start.elapsed();
    assert_eq!(
        out.status.code(),
        Some(106),
        "a SIGTERM-ignoring child is still a timeout, torn down by the hard kill"
    );
    // Deadline alone would end near ~1s; honoring the 3s grace pushes it past ~3.5s.
    assert!(
        elapsed >= Duration::from_millis(3500),
        "grace was not honored: the run ended after {elapsed:?}, expected >= ~3.5s"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--grace` is an *upper bound*, not a mandatory delay: a tree that dies
/// promptly after the soft signal (no `trap` here, so `sleep` obeys the default
/// `SIGTERM` disposition and exits immediately) must not hold the run for
/// anywhere near the full window. Asserted through a loose upper bound on total
/// elapsed time — never a tight/exact tail — so the poll-step granularity of the
/// early-exit check cannot make this test flaky.
#[cfg(unix)]
#[test]
fn grace_ends_early_once_the_tree_has_already_emptied() {
    let dir = scratch("grace-early");
    let start = std::time::Instant::now();
    let out = run_with_flags(
        &dir,
        &[],
        &["--timeout", "200ms", "--grace", "5s"],
        shell_inline("sleep 300"),
    );
    let elapsed = start.elapsed();
    assert_eq!(
        out.status.code(),
        Some(106),
        "still a timeout, even though the child died to the soft signal rather than naturally"
    );
    // An unconditional full grace would run to roughly timeout(~0.2s) + grace(5s)
    // ~= 5.2s; observing the emptied tree early must end the run in a small
    // fraction of that — generous enough to absorb CI scheduling jitter, still
    // far short of the un-shortened window.
    assert!(
        elapsed < Duration::from_secs(2),
        "the grace window was not cut short: the run took {elapsed:?}, expected well under \
         the full ~5.2s (timeout + grace)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `Ctrl-C` mid-run is a **distinguishable** ending: the runner exits with the
/// reserved `CANCELLED` code (107 — not a timeout, not a child code) and tears the
/// tree down. Unix-only: it delivers a real `SIGINT` (the interactive Ctrl-C) to
/// the runner process; an isolated Ctrl-C cannot be sent to a single child on
/// Windows, so that platform is covered by the honest-message and unit tests.
#[cfg(unix)]
#[test]
fn cancel_via_ctrl_c_reports_the_cancel_code_and_tears_down_the_tree() {
    use std::process::Stdio;

    let dir = scratch("cancel");
    let heartbeat = dir.join("heartbeat.txt");
    let grandchild = write_grandchild_script(&dir);
    let root = write_sleeping_root_script(&dir);

    let child = common::command_with_flags(
        &dir,
        &[
            ("HB", heartbeat.as_path()),
            ("GRANDCHILD", grandchild.as_path()),
        ],
        &["--grace", "1s"],
        vec!["/bin/sh".to_string(), path_arg(&root)],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn the runner");

    // Let the grandchild start heartbeating so the SIGINT lands mid-run.
    wait_until(|| file_len(&heartbeat) > 0, Duration::from_secs(10));

    // Deliver the interactive Ctrl-C the runner listens for — to the runner alone
    // (its pid), not a process group, so only the runner sees it.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(rc, 0, "failed to deliver SIGINT to the runner");

    let out = child.wait_with_output().expect("runner did not exit");
    assert_eq!(
        out.status.code(),
        Some(107),
        "a Ctrl-C cancel must exit with the reserved CANCELLED code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cancelled"),
        "the cancel must be explained on stderr: {stderr:?}"
    );

    // The tree must be gone: the heartbeat cannot grow after the runner returned.
    let size_at_return = file_len(&heartbeat);
    assert!(
        size_at_return > 0,
        "the grandchild must have heartbeat before the cancel"
    );
    sleep(Duration::from_secs(3));
    let size_later = file_len(&heartbeat);
    assert_eq!(
        size_later, size_at_return,
        "a descendant survived the cancel teardown (grew from {size_at_return} to {size_later})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A shell or supervisor can deliberately launch the runner with `SIGINT` ignored.
/// The runner must preserve that inherited policy instead of installing its Ctrl-C
/// handler and making the run interruptible behind the launcher's back.
#[cfg(unix)]
#[test]
fn inherited_sigint_ignore_is_preserved_by_the_runner() {
    use std::os::unix::process::CommandExt;

    let dir = scratch("sigint-ignore");
    let mut command = common::command_with_flags(
        &dir,
        &[],
        &[],
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 300".to_string(),
        ],
    );
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    // SAFETY: this pre-exec hook makes one async-signal-safe libc call in the child
    // between fork and exec. It changes only the child runner's inherited SIGINT
    // disposition, which is the behavior under test.
    unsafe {
        command.pre_exec(|| {
            if libc::signal(libc::SIGINT, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .expect("spawn the runner with SIGINT ignored");

    wait_until(|| file_len(&events_path(&dir)) > 0, Duration::from_secs(10));
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(rc, 0, "failed to deliver the inherited-ignored SIGINT");
    sleep(Duration::from_millis(500));
    assert!(
        child.try_wait().expect("probe runner state").is_none(),
        "the runner must remain live after an inherited-ignored SIGINT"
    );

    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0, "failed to stop the runner after the assertion");
    let out = child
        .wait_with_output()
        .expect("runner did not exit on SIGTERM");
    assert_eq!(out.status.code(), Some(107));
    let cancelled = read_run_events(&dir)
        .into_iter()
        .find(|event| event["event"] == "cancelled")
        .expect("the cleanup SIGTERM is reported");
    assert_eq!(cancelled["source"], "sigterm");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A `SIGTERM` mid-run — the *standard external stop* (`kill <pid>`, `systemctl
/// stop`, a cancelled CI job, a supervisor's shutdown timeout) — must be a
/// **first-class cancel**, not an abrupt death of the runner: the same reserved
/// `CANCELLED` code (107), the same complete terminal JSONL sequence, and the same
/// full teardown of the tree. Before the runner caught this signal its default
/// disposition killed the runner outright, so none of that happened — no terminal
/// events, and no explicit kill of the container, whose abrupt-owner-death reap
/// covers only the direct child on Linux (`PDEATHSIG`) and nothing on macOS/BSD
/// (K-005). The detached grandchild here is exactly the descendant that reap would
/// miss, which is what makes the bystander check load-bearing rather than decorative.
///
/// Unix-only: it delivers a real `SIGTERM` to the runner process alone (its pid, not
/// a process group), so only the runner sees it — the child must be reaped by the
/// runner's teardown, not by the signal itself.
#[cfg(unix)]
#[test]
fn cancel_via_sigterm_reports_the_cancel_code_and_tears_down_the_tree() {
    use std::process::Stdio;

    let dir = scratch("sigterm");
    let heartbeat = dir.join("heartbeat.txt");
    let grandchild = write_grandchild_script(&dir);
    let root = write_sleeping_root_script(&dir);

    let child = common::command_with_flags(
        &dir,
        &[
            ("HB", heartbeat.as_path()),
            ("GRANDCHILD", grandchild.as_path()),
        ],
        &["--grace", "1s"],
        vec!["/bin/sh".to_string(), path_arg(&root)],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn the runner");

    // Let the grandchild start heartbeating so the SIGTERM lands mid-run, with a
    // live descendant to tear down.
    wait_until(|| file_len(&heartbeat) > 0, Duration::from_secs(10));

    // The external stop: SIGTERM to the runner alone (its pid), not a process group.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0, "failed to deliver SIGTERM to the runner");

    let out = child.wait_with_output().expect("runner did not exit");
    assert_eq!(
        out.status.code(),
        Some(107),
        "a SIGTERM must exit with the reserved CANCELLED code, like any other cancel"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("run cancelled (SIGTERM)"),
        "the stderr line must name the signal that actually stopped the run, not a \
         Ctrl-C that never happened: {stderr:?}"
    );

    // The full terminal sequence must be present and in order — this is what a plain
    // `kill` used to skip entirely.
    let events = read_run_events(&dir);
    let tags: Vec<&str> = events
        .iter()
        .filter_map(|event| event["event"].as_str())
        .collect();
    let cancelled = events
        .iter()
        .find(|event| event["event"] == "cancelled")
        .unwrap_or_else(|| panic!("a SIGTERM must write a `cancelled` event: {tags:?}"));
    assert_eq!(
        cancelled["source"], "sigterm",
        "the cancel must be attributed to the signal that arrived: {cancelled}"
    );
    let position = |tag: &str| {
        tags.iter()
            .position(|found| *found == tag)
            .unwrap_or_else(|| panic!("the stream must contain a `{tag}` event: {tags:?}"))
    };
    let (cancel_at, started_at, finished_at) = (
        position("cancelled"),
        position("cleanup_started"),
        position("cleanup_finished"),
    );
    assert!(
        cancel_at < started_at && started_at < finished_at,
        "the reason event must bracket the teardown pair: {tags:?}"
    );
    let terminal = events.last().expect("a terminal event");
    assert_eq!(
        terminal["event"], "runner_exit",
        "`runner_exit` is always the last line: {tags:?}"
    );
    assert_eq!(terminal["source"], "cancelled");
    assert_eq!(terminal["code"], 107);
    assert_eq!(
        terminal["child_code"],
        Value::Null,
        "a runner-imposed ending forwards no child code"
    );

    // And the headline guarantee: the whole tree is gone. The detached grandchild
    // cannot grow its heartbeat after the runner returned. Baseline is read *after*
    // the runner exited, so a stale pre-teardown value cannot mask a survivor (K-012).
    let size_at_return = file_len(&heartbeat);
    assert!(
        size_at_return > 0,
        "the grandchild must have heartbeat before the SIGTERM"
    );
    sleep(Duration::from_secs(3));
    let size_later = file_len(&heartbeat);
    assert_eq!(
        size_later, size_at_return,
        "a descendant survived the SIGTERM teardown (grew from {size_at_return} to \
         {size_later})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `SIGHUP` — the controlling terminal went away (a closed terminal, a dropped SSH
/// session) — is the sibling of the `SIGTERM` case above and takes the very same
/// path, distinguished only by the `cancelled` event's `source`. Kept as its own
/// (lighter) test rather than folded into the one above so a regression that wires
/// only one of the two signals cannot hide.
#[cfg(unix)]
#[test]
fn cancel_via_sighup_is_reported_as_its_own_source() {
    use std::process::Stdio;

    let dir = scratch("sighup");
    let heartbeat = dir.join("heartbeat.txt");
    let grandchild = write_grandchild_script(&dir);
    let root = write_sleeping_root_script(&dir);

    let child = common::command_with_flags(
        &dir,
        &[
            ("HB", heartbeat.as_path()),
            ("GRANDCHILD", grandchild.as_path()),
        ],
        &["--grace", "1s"],
        vec!["/bin/sh".to_string(), path_arg(&root)],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn the runner");

    wait_until(|| file_len(&heartbeat) > 0, Duration::from_secs(10));

    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGHUP) };
    assert_eq!(rc, 0, "failed to deliver SIGHUP to the runner");

    let out = child.wait_with_output().expect("runner did not exit");
    assert_eq!(
        out.status.code(),
        Some(107),
        "a SIGHUP shares the reserved CANCELLED code with the other stop signals"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("run cancelled (SIGHUP)"),
        "the stderr line must name SIGHUP: {stderr:?}"
    );

    let events = read_run_events(&dir);
    let cancelled = events
        .iter()
        .find(|event| event["event"] == "cancelled")
        .expect("a SIGHUP must write a `cancelled` event");
    assert_eq!(
        cancelled["source"], "sighup",
        "a SIGHUP is neither a Ctrl-C nor a SIGTERM: {cancelled}"
    );
    let terminal = events.last().expect("a terminal event");
    assert_eq!(terminal["event"], "runner_exit");
    assert_eq!(terminal["source"], "cancelled");
    assert_eq!(terminal["code"], 107);

    let size_at_return = file_len(&heartbeat);
    assert!(
        size_at_return > 0,
        "the grandchild must have heartbeat before the SIGHUP"
    );
    sleep(Duration::from_secs(3));
    assert_eq!(
        file_len(&heartbeat),
        size_at_return,
        "a descendant survived the SIGHUP teardown"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A real `CTRL_BREAK_EVENT` — generated for the runner's own console process
/// group via `GenerateConsoleCtrlEvent`, not merely simulated — must be a
/// first-class cancel: the reserved `CANCELLED` code, the `cancelled` event's
/// `source` `ctrl_break`, the honest stderr headline, and the full terminal
/// JSONL sequence (T-195, the Windows sibling of the Unix `SIGTERM`/`SIGHUP`
/// proofs above).
///
/// `GenerateConsoleCtrlEvent` can target a *single* process (group) only for
/// `CTRL_BREAK_EVENT`, and only when that process was created with
/// `CREATE_NEW_PROCESS_GROUP` (its own pid then *is* the group id) — otherwise the
/// event broadcasts to every process sharing this test's console, including the
/// test harness itself. So the runner is spawned with that flag and the event is
/// generated against its pid alone, leaving this test process unaffected.
///
/// **What the heartbeat check below does and does not prove.** The grandchild is
/// started (via `start /b`) *inside* the runner's own process group — it has no
/// `CREATE_NEW_PROCESS_GROUP` of its own — so the same `CTRL_BREAK` broadcast that
/// reaches the runner also reaches the grandchild directly. A stopped heartbeat is
/// therefore not, by itself, proof that the *runner's* teardown reaped it; it is
/// kept as a coarse regression guard (a teardown that never ran at all would leave
/// the grandchild heartbeating past the runner's return, which this still catches).
/// The real proof that the cancel path — not the OS event alone — ran is the
/// terminal code/`source`/JSONL-sequence assertions above.
///
/// **What the terminal `source` assertion below also guards against.** If the
/// whole tree happened to die from the `CTRL_BREAK` broadcast itself faster than
/// the runner could observe and report the signal, the race in `run_async` could
/// in principle resolve as a plain child exit instead of a cancel. `terminal["source"]
/// == "cancelled"` (not `"child_exit"`) catches that as a hard assertion failure,
/// not a silent flake — if this ever becomes flaky, that is the race to look at.
///
/// **Console requirement.** `GenerateConsoleCtrlEvent` only works when this test
/// process itself is attached to a console shared with the target process group —
/// true when `cargo test` runs interactively, not guaranteed in every CI
/// environment (see `allocate_fresh_console` in `src/bin/e2e_helper.rs` for the
/// same "may inherit a console locally and no console in CI" caveat). Rather than
/// fail the build over a test-harness limitation with no console to deliver
/// through, this test degrades to an honest skip with a diagnostic in that case —
/// real CTRL_BREAK-delivery coverage is then whatever the environment happens to
/// provide; the CLOSE/LOGOFF/SHUTDOWN unit coverage elsewhere in this crate still
/// exercises the branch-selection/mapping/grace-clamp logic without needing one.
#[cfg(windows)]
#[test]
fn cancel_via_ctrl_break_reports_the_cancel_code_and_tears_down_the_tree() {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, GetConsoleProcessList,
    };
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

    let dir = scratch("ctrl_break");
    let heartbeat = dir.join("heartbeat.txt");
    let grandchild = write_grandchild_script(&dir);
    let root = write_sleeping_root_script(&dir);

    let mut console_pid = 0;
    // A headless cargo/IDE/agent process cannot deliver CTRL_BREAK. Detect that
    // before launching the console child: starting it first would make Windows
    // Terminal create a delegated pane that survives the intentional test kill as
    // an error pane. Interactive runs still exercise the real delivery path.
    if unsafe { GetConsoleProcessList(&mut console_pid, 1) } == 0 {
        eprintln!(
            "skipping cancel_via_ctrl_break_reports_the_cancel_code_and_tears_down_the_tree: \
             this test process has no console to deliver CTRL_BREAK through"
        );
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    let mut cmd = common::command_with_inherited_console_flags(
        &dir,
        &[
            ("HB", heartbeat.as_path()),
            ("GRANDCHILD", grandchild.as_path()),
        ],
        &["--grace", "1s"],
        vec!["cmd".to_string(), "/c".to_string(), path_arg(&root)],
    );
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the runner");

    // Let the grandchild start heartbeating so CTRL_BREAK lands mid-run, with a
    // live descendant to tear down.
    wait_until(|| file_len(&heartbeat) > 0, Duration::from_secs(10));

    let pid = child.id();
    // SAFETY: a plain FFI call with valid, POD arguments (`CTRL_BREAK_EVENT` is a
    // fixed constant, `pid` a `u32` this process itself just read back from
    // `Child::id`). `pid` names the runner's own process group — it was spawned
    // with `CREATE_NEW_PROCESS_GROUP`, so its own pid *is* the group id — so only
    // the runner receives this event, not this test process.
    let generated = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    if generated == 0 {
        // This test process has no console attached to deliver the event through
        // (see the doc comment above) — an environment limitation, not a runner
        // defect. Tear down the still-running child honestly instead of leaking
        // it, and skip rather than fail the build.
        eprintln!(
            "skipping cancel_via_ctrl_break_reports_the_cancel_code_and_tears_down_the_tree: \
             GenerateConsoleCtrlEvent failed ({}) — this test process has no console to \
             deliver CTRL_BREAK through",
            std::io::Error::last_os_error()
        );
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    let out = child.wait_with_output().expect("runner did not exit");
    assert_eq!(
        out.status.code(),
        Some(107),
        "a CTRL_BREAK cancel must exit with the reserved CANCELLED code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("run cancelled (Ctrl-Break)"),
        "the stderr line must name Ctrl-Break: {stderr:?}"
    );

    // The full terminal sequence must be present, with the cancel attributed to
    // CTRL_BREAK specifically — not flattened onto `ctrl_c`.
    let events = read_run_events(&dir);
    let tags: Vec<&str> = events
        .iter()
        .filter_map(|event| event["event"].as_str())
        .collect();
    let cancelled = events
        .iter()
        .find(|event| event["event"] == "cancelled")
        .unwrap_or_else(|| panic!("a CTRL_BREAK must write a `cancelled` event: {tags:?}"));
    assert_eq!(
        cancelled["source"], "ctrl_break",
        "the cancel must be attributed to CTRL_BREAK: {cancelled}"
    );
    let terminal = events.last().expect("a terminal event");
    assert_eq!(terminal["event"], "runner_exit");
    assert_eq!(terminal["source"], "cancelled");
    assert_eq!(terminal["code"], 107);

    // And the headline guarantee: the whole tree is gone. The detached grandchild
    // cannot grow its heartbeat after the runner returned.
    let size_at_return = file_len(&heartbeat);
    assert!(
        size_at_return > 0,
        "the grandchild must have heartbeat before the CTRL_BREAK"
    );
    sleep(Duration::from_secs(3));
    let size_later = file_len(&heartbeat);
    assert_eq!(
        size_later, size_at_return,
        "a descendant survived the CTRL_BREAK teardown (grew from {size_at_return} to {size_later})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Poll `cond` until it holds or `timeout` elapses (then panic). A tiny spin used
/// by the cancel tests to wait for the grandchild to come alive.
fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) {
    let start = std::time::Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < timeout,
            "condition was not met within {timeout:?}"
        );
        sleep(Duration::from_millis(50));
    }
}

/// The binary path is stable — a cheap guard that the fixture points at a real
/// executable before the heavier scenarios run.
#[test]
fn binary_under_test_exists() {
    assert!(
        Path::new(bin()).is_file(),
        "the built binary should exist at {}",
        bin()
    );
}
