//! Through-the-binary tests for the `events` subcommand — the read-back half of
//! the JSONL lifecycle stream (`AGENTS.md`, "Testing tiers"). `tests/events.rs`
//! proves the binary *writes* the stream; this file proves the same binary reads it
//! back: resolving the locator through the registry or from an explicit path,
//! rendering it, passing it through untouched, following it to its terminal event,
//! and checking it against the schema it embeds.
//!
//! Every test drives the built binary and points it at an isolated scratch registry
//! via `PROCESSKIT_CLI_REGISTRY_DIR`, so it never touches the real per-user registry
//! and parallel tests never collide.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{bin, command_with_flags, events_path, scratch, shell_inline};
use processkit_cli::exit;

/// The scratch registry a test's runner and its `events` client share.
fn registry_dir(scratch_dir: &Path) -> PathBuf {
    scratch_dir.join("registry")
}

/// How many records the scratch registry holds right now. The readiness signal for
/// a by-`run-id` test: the runner creates its `--jsonl` file *before* it registers,
/// so the file's existence is not evidence that the id resolves yet.
fn record_count(registry: &Path) -> usize {
    match std::fs::read_dir(registry) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .count(),
        Err(_) => 0,
    }
}

/// Run the `events` client against the same scratch registry as the run under test.
fn events(registry: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .arg("events")
        .args(args)
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .expect("spawn the events client")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone())
        .expect("stdout is UTF-8")
        .replace("\r\n", "\n")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone())
        .expect("stderr is UTF-8")
        .replace("\r\n", "\n")
}

/// A child that outlives the whole test, so a live-run observation can never race
/// the fixture's own window shut. The tests that use it end the run deliberately
/// ([`end_run`]) once they have observed what they came for, rather than waiting it
/// out — `tests/registry.rs`'s own fixture comment records that a window sized to
/// "long enough on an idle host" flakes reproducibly under `cargo test`
/// parallelism, so this one is not sized at all.
fn long_lived_child() -> Vec<String> {
    if cfg!(windows) {
        shell_inline("ping -n 31 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 30")
    }
}

/// End a live fixture run through the control plane, so a test's timing is its own
/// rather than its child's.
fn end_run(registry: &Path, run_id: &str) {
    let out = Command::new(bin())
        .args(["kill", "--run-id", run_id])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .expect("spawn the kill client");
    assert_eq!(
        out.status.code(),
        Some(0),
        "ending the fixture run succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run a command to completion with `--run-id`, leaving its finished stream behind.
fn completed_run(dir: &Path, registry: &Path, run_id: &str) -> PathBuf {
    let out = command_with_flags(
        dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry)],
        &["--run-id", run_id],
        shell_inline("exit 0"),
    )
    .output()
    .expect("run the fixture command");
    assert_eq!(
        out.status.code(),
        Some(0),
        "the fixture run succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    events_path(dir)
}

fn wait_until(mut ready: impl FnMut() -> bool, bound: Duration) {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        sleep(Duration::from_millis(25));
    }
    assert!(ready(), "condition not reached within {bound:?}");
}

/// The default form: every event of a finished run, one human-readable line each,
/// ending with the terminal event — the "what happened" an operator reads.
#[test]
fn the_default_form_renders_every_event_of_a_finished_run() {
    let dir = scratch("events-render");
    let registry = registry_dir(&dir);
    let stream = completed_run(&dir, &registry, "rendered-run");

    let out = events(&registry, &["--file", &stream.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    let rendered = stdout(&out);
    let lines: Vec<&str> = rendered.lines().collect();
    let raw = std::fs::read_to_string(&stream).expect("read the stream");
    assert_eq!(
        lines.len(),
        raw.lines().filter(|line| !line.trim().is_empty()).count(),
        "one rendered line per event: {rendered}"
    );
    assert!(
        lines[0].contains("run_started") && lines[0].contains("rendered-run"),
        "the first line is the run's start: {}",
        lines[0]
    );
    let last = lines.last().expect("a terminal line");
    assert!(
        last.contains("runner_exit") && last.contains("source=child_exit"),
        "the last line is the terminal event: {last}"
    );
    // Every rendered line starts with the event's own timestamp, so a stream reads
    // as a chronology rather than an undifferentiated wall of fields.
    for line in &lines {
        assert!(
            line.starts_with("20") && line.contains('Z'),
            "each line leads with its RFC 3339 timestamp: {line}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--json` hands back the runner's own bytes, byte for byte. Proven the only way
/// that means anything: stdout is compared against the file itself, and the
/// *field order* of a line is compared against what a re-serialization would have
/// produced (this binary's own JSON map sorts keys, so a round trip through a typed
/// struct or a `Value` would visibly reorder them — K-092).
#[test]
fn json_mode_passes_the_runners_own_bytes_through() {
    let dir = scratch("events-json");
    let registry = registry_dir(&dir);
    let stream = completed_run(&dir, &registry, "passthrough-run");

    let out = events(&registry, &["--file", &stream.to_string_lossy(), "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    let raw = std::fs::read_to_string(&stream)
        .expect("read the stream")
        .replace("\r\n", "\n");
    assert_eq!(
        stdout(&out),
        raw,
        "--json is a byte-for-byte pass-through of the stream"
    );

    let first = raw.lines().next().expect("a first event");
    let reserialized = serde_json::to_string(
        &serde_json::from_str::<serde_json::Value>(first).expect("the line parses"),
    )
    .expect("the line re-serializes");
    assert_ne!(
        first, reserialized,
        "the fixture must actually distinguish the two: a re-serialization reorders \
         this line's fields, so the equality above is real evidence"
    );
    assert!(
        stdout(&out).lines().next() == Some(first),
        "the emitted line is the runner's, not a re-serialization"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--run-id` resolves the locator through the registry while the record exists,
/// and `--file` is the documented way to read the very same stream once the record
/// is gone — the two halves of the locator contract, proven against one run.
#[test]
fn a_stream_resolves_by_run_id_while_registered_and_by_file_afterwards() {
    let dir = scratch("events-locator");
    let registry = registry_dir(&dir);
    let stream = events_path(&dir);

    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "locatable"],
        long_lived_child(),
    )
    .spawn()
    .expect("spawn the runner");

    // Resolvable by id as soon as the record — and the stream it names — exist.
    wait_until(
        || record_count(&registry) == 1 && stream.is_file(),
        Duration::from_secs(20),
    );
    let live = events(&registry, &["--run-id", "locatable"]);
    assert_eq!(
        live.status.code(),
        Some(0),
        "a registered run resolves by id; stderr: {}",
        stderr(&live)
    );
    assert!(
        stdout(&live).contains("run_started"),
        "the resolved stream is the run's own: {}",
        stdout(&live)
    );

    // End the run on purpose rather than waiting out its fixture window: the record
    // (and with it the id's resolvability) goes away when the run does, and this
    // test is about what happens *after* that, not about how long a ping takes.
    end_run(&registry, "locatable");
    child.wait().expect("await the runner");
    wait_until(|| record_count(&registry) == 0, Duration::from_secs(20));

    // The finished run removed its record, so the id no longer names a stream — and
    // the failure says so with the shared `CONTROL` verdict, pointing at `--file`.
    let gone = events(&registry, &["--run-id", "locatable"]);
    assert_eq!(
        gone.status.code(),
        Some(i32::from(exit::CONTROL)),
        "a reaped record is an unresolvable id, not an empty stream"
    );
    assert!(
        stderr(&gone).contains("--file"),
        "the refusal names the escape hatch: {}",
        stderr(&gone)
    );

    // …which reads the same, now complete, stream.
    let by_file = events(&registry, &["--file", &stream.to_string_lossy()]);
    assert_eq!(
        by_file.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&by_file)
    );
    assert!(
        stdout(&by_file).contains("runner_exit"),
        "the stream is readable after its record is gone: {}",
        stdout(&by_file)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--follow` on a live run keeps reading while the run keeps running, and returns
/// as soon as the terminal event lands — the two halves of the follow contract.
///
/// Both are established by *observation*, not by a sleep: the follower's first
/// rendered line proves it is attached and reading a run that is still going (the
/// child outlives the whole test), and only then is the run deliberately ended, so
/// the follow's return can be attributed to the terminal event rather than to a
/// fixture window quietly running out. That also keeps the test's cost the run's
/// teardown rather than the child's whole lifetime.
#[test]
fn follow_runs_until_the_terminal_event_and_then_returns() {
    let dir = scratch("events-follow");
    let registry = registry_dir(&dir);
    let stream = events_path(&dir);

    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "followed"],
        long_lived_child(),
    )
    .spawn()
    .expect("spawn the runner");

    wait_until(
        || record_count(&registry) == 1 && stream.is_file(),
        Duration::from_secs(20),
    );

    let mut follower = Command::new(bin())
        .args(["events", "--run-id", "followed", "--follow"])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", &registry)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the follower");

    // Blocks until the follower has actually rendered something: from here on it is
    // demonstrably attached to a live run and has *not* returned, which is the half
    // of the contract a completed-stream test could never show.
    let mut rendered = String::new();
    let mut out = BufReader::new(follower.stdout.take().expect("piped stdout"));
    out.read_line(&mut rendered).expect("read the first event");
    assert!(
        rendered.contains("run_started"),
        "the follower starts at the beginning of the stream: {rendered}"
    );

    end_run(&registry, "followed");
    out.read_to_string(&mut rendered)
        .expect("read the rest of the followed stream");
    let status = follower.wait().expect("await the follower");
    child.wait().expect("await the runner");

    assert_eq!(
        status.code(),
        Some(0),
        "a follow that reaches the terminal event succeeds"
    );
    assert!(
        rendered.contains("runner_exit"),
        "the follow ran through to the stream's own end: {rendered}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A schema-invalid `runner_exit` is still an ordinary event for a live follow:
/// only the later, complete terminal line may end the follower.
#[test]
fn follow_ignores_a_malformed_runner_exit_until_a_valid_terminal_arrives() {
    let dir = scratch("events-follow-malformed-terminal");
    let registry = registry_dir(&dir);
    let stream = events_path(&dir);

    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "malformed-terminal"],
        long_lived_child(),
    )
    .spawn()
    .expect("spawn the runner");

    wait_until(
        || {
            record_count(&registry) == 1
                && stream.is_file()
                && std::fs::metadata(&stream).is_ok_and(|metadata| metadata.len() > 0)
        },
        Duration::from_secs(20),
    );

    let mut follower = Command::new(bin())
        .args(["events", "--run-id", "malformed-terminal", "--follow"])
        .env("PROCESSKIT_CLI_REGISTRY_DIR", &registry)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the follower");

    let mut rendered = String::new();
    let mut out = BufReader::new(follower.stdout.take().expect("piped stdout"));
    out.read_line(&mut rendered).expect("read the first event");
    assert!(
        rendered.contains("run_started"),
        "the follower starts at the beginning of the stream: {rendered}"
    );

    let mut appended = std::fs::OpenOptions::new()
        .append(true)
        .open(&stream)
        .expect("open the stream for the malformed event");
    let malformed_terminal = r#"{"schema_version":1,"event":"runner_exit"}"#;
    writeln!(appended, "{malformed_terminal}").expect("append the malformed terminal");

    // Wait until the malformed line has actually reached the follower before ending
    // the run; otherwise this test could pass without exercising the old predicate.
    let mut line = String::new();
    loop {
        line.clear();
        out.read_line(&mut line).expect("read the malformed event");
        assert!(!line.is_empty(), "the live follower remains attached");
        rendered.push_str(&line);
        if line.contains("runner_exit") {
            break;
        }
    }

    end_run(&registry, "malformed-terminal");
    out.read_to_string(&mut rendered)
        .expect("read the valid terminal");
    let status = follower.wait().expect("await the follower");
    child.wait().expect("await the runner");

    assert_eq!(
        status.code(),
        Some(0),
        "a later valid terminal ends the follow"
    );
    assert!(
        rendered.matches("runner_exit").count() >= 2 && rendered.contains("source=control_kill"),
        "the malformed and later valid terminal were both consumed: {rendered}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--follow` on a stream that no live run is writing stops on its own, bounded,
/// and says why — the "a runner killed abruptly never writes its terminal event"
/// case, which must never become a hang.
#[test]
fn follow_stops_bounded_on_a_stream_with_no_terminal_event() {
    let dir = scratch("events-follow-truncated");
    let registry = registry_dir(&dir);
    let stream = completed_run(&dir, &registry, "truncated-run");

    // Drop the terminal event: what an abruptly killed runner leaves behind.
    let complete = std::fs::read_to_string(&stream).expect("read the stream");
    let mut lines: Vec<&str> = complete.lines().collect();
    let terminal = lines.pop().expect("a terminal line");
    assert!(
        terminal.contains("runner_exit"),
        "the fixture drops the end"
    );
    let truncated_path = dir.join("truncated.jsonl");
    std::fs::write(&truncated_path, format!("{}\n", lines.join("\n"))).expect("write the fixture");

    let started = Instant::now();
    let out = events(
        &registry,
        &["--file", &truncated_path.to_string_lossy(), "--follow"],
    );
    let elapsed = started.elapsed();

    assert_eq!(
        out.status.code(),
        Some(0),
        "an incomplete stream is a result, not a failure; stderr: {}",
        stderr(&out)
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "the follow is bounded by the run's absence, never unbounded: {elapsed:?}"
    );
    assert!(
        stderr(&out).contains("without a terminal"),
        "the incomplete stream is explained, not silently accepted: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("run_started"),
        "everything that was there is still reported: {}",
        stdout(&out)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--validate` agrees with the schema on a stream this very binary wrote — and on
/// the shipped golden fixture, so the first-party checker and the test tier's own
/// validator are held to the same document.
#[test]
fn validate_accepts_a_conforming_stream_and_the_golden_fixture() {
    let dir = scratch("events-validate-ok");
    let registry = registry_dir(&dir);
    let stream = completed_run(&dir, &registry, "conforming-run");

    let out = events(
        &registry,
        &["--file", &stream.to_string_lossy(), "--validate"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "a stream this binary wrote conforms to the schema it publishes; stderr: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("all valid"),
        "the summary says so: {}",
        stdout(&out)
    );

    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/schema/v1/events.jsonl");
    let out = events(
        &registry,
        &["--file", &golden.to_string_lossy(), "--validate"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "the golden fixture validates through the binary too; stderr: {}",
        stderr(&out)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The gate an adapter author actually wants: a non-conforming line fails with the
/// reserved `EVENTS_INVALID` code and a report naming the line and what it violated
/// — never a silent pass, and never a generic error.
#[test]
fn validate_fails_closed_on_a_non_conforming_line() {
    let dir = scratch("events-validate-bad");
    let registry = registry_dir(&dir);
    let stream = completed_run(&dir, &registry, "broken-run");

    let complete = std::fs::read_to_string(&stream).expect("read the stream");
    let mut lines: Vec<String> = complete.lines().map(str::to_string).collect();
    // A `runner_exit` whose `code` is a string: valid JSON, wrong shape.
    lines.insert(
        1,
        r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"runner_exit","code":"seven","source":"child_exit","child_code":null}"#
            .to_string(),
    );
    lines.insert(2, "this line is not JSON at all".to_string());
    let broken = dir.join("broken.jsonl");
    std::fs::write(&broken, format!("{}\n", lines.join("\n"))).expect("write the fixture");

    let out = events(
        &registry,
        &["--file", &broken.to_string_lossy(), "--validate"],
    );
    assert_eq!(
        out.status.code(),
        Some(i32::from(exit::EVENTS_INVALID)),
        "a non-conforming stream fails closed; stdout: {}",
        stdout(&out)
    );

    let report = stdout(&out);
    assert!(
        report.contains("line 2: /code:"),
        "the report names the offending line and field: {report}"
    );
    assert!(
        report.contains("line 3: not valid JSON"),
        "a line that is not JSON is a violation too: {report}"
    );
    assert!(
        report.contains("2 invalid"),
        "the summary tallies exactly the two bad lines: {report}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A line that is not an event never reaches stdout in `--json` mode: a consumer
/// piping stdout into a JSONL parser can rely on every line being parseable, and
/// the dropped line is reported on stderr rather than lost.
#[test]
fn a_malformed_line_is_reported_on_stderr_and_kept_out_of_machine_output() {
    let dir = scratch("events-malformed");
    let registry = registry_dir(&dir);
    let stream = completed_run(&dir, &registry, "noisy-run");

    let complete = std::fs::read_to_string(&stream).expect("read the stream");
    let mut lines: Vec<String> = complete.lines().map(str::to_string).collect();
    lines.insert(1, "{ this is not json".to_string());
    let noisy = dir.join("noisy.jsonl");
    std::fs::write(&noisy, format!("{}\n", lines.join("\n"))).expect("write the fixture");

    let out = events(&registry, &["--file", &noisy.to_string_lossy(), "--json"]);
    assert_eq!(out.status.code(), Some(0), "one bad line is not a failure");
    for line in stdout(&out).lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|err| panic!("every stdout line parses as JSON ({err}): {line}"));
    }
    assert!(
        stderr(&out).contains("line 2: not valid JSON"),
        "the dropped line is named, not silently skipped: {}",
        stderr(&out)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A stream that cannot be opened at all is a setup failure the caller can act on —
/// distinct from "the stream is invalid" (`EVENTS_INVALID`) and from "the id names
/// no stream" (`CONTROL`), so a CI job can tell the three apart.
#[test]
fn an_unreadable_stream_is_a_setup_failure() {
    let dir = scratch("events-missing");
    let registry = registry_dir(&dir);
    let missing = dir.join("nowhere.jsonl");

    let out = events(&registry, &["--file", &missing.to_string_lossy()]);
    assert_eq!(
        out.status.code(),
        Some(i32::from(exit::SETUP)),
        "an unopenable stream is a setup condition; stderr: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("could not open the events stream"),
        "the diagnostic names what failed: {}",
        stderr(&out)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The parse-time contract, through the binary: exactly one locator is required,
/// the two are mutually exclusive with no silent precedence, and the two output
/// modes cannot be combined — each rejected as an ordinary `USAGE` form error.
#[test]
fn malformed_invocations_are_usage_errors() {
    let dir = scratch("events-usage");
    let registry = registry_dir(&dir);

    for argv in [
        vec![],
        vec!["--run-id", "r1", "--file", "events.jsonl"],
        vec!["--file", "events.jsonl", "--json", "--validate"],
        vec!["--follow"],
    ] {
        let out = events(&registry, &argv);
        assert_eq!(
            out.status.code(),
            Some(i32::from(exit::USAGE)),
            "`events {argv:?}` must be a usage error; stderr: {}",
            stderr(&out)
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
