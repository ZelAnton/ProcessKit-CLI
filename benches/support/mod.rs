//! Shared fixtures for the through-the-binary benchmarks
//! (`echo_overhead_bench.rs`, `startup_latency_bench.rs`). Mirrors the pattern
//! `tests/common/mod.rs` uses for the integration tests: each `benches/*.rs`
//! file is its own crate, so this module is pulled in via `#[path = …]` rather
//! than a normal `mod` declaration (see the bottom of each bench file).
//!
//! Not itself a `[[bench]]` target (no `name`/`path` entry in `Cargo.toml`
//! references it directly) — only ever compiled as a submodule of a bench that
//! opts in.

#![allow(dead_code)] // Each bench file uses a subset of these.

use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime};

/// Absolute path to the freshly built `processkit-cli` binary under
/// benchmark — the same binary the through-the-binary integration tests
/// (`tests/common::bin`) drive.
pub fn runner_bin() -> &'static str {
    env!("CARGO_BIN_EXE_processkit-cli")
}

/// Absolute path to the `bench_emit` worker (`src/bin/bench_emit.rs`, built
/// alongside these benches under the `bench` feature): writes an exact byte
/// count to stdout, so the echo-overhead scenario measures the runner's own
/// pump/echo/capture cost against a precisely-sized child.
pub fn emit_bin() -> &'static str {
    env!("CARGO_BIN_EXE_bench_emit")
}

/// A scratch directory scoped to one benchmark group, removed on drop. Reused
/// across a group's iterations (its files are truncated/overwritten each run,
/// same as the runner does in production) rather than recreated per
/// iteration, so directory setup never pollutes the timed measurement.
pub struct Scratch {
    pub dir: PathBuf,
}

impl Scratch {
    pub fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "processkit-cli-bench-{tag}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create bench scratch dir");
        Self { dir }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A `bench_emit --bytes <bytes>` invocation, direct (no runner in between):
/// the echo-overhead baseline.
pub fn direct_command(bytes: u64) -> Command {
    let mut cmd = Command::new(emit_bin());
    cmd.args(["--bytes", &bytes.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    cmd
}

/// A `processkit-cli run [--capture-dir <dir>] -- bench_emit --bytes <bytes>`
/// invocation. `capture` toggles `--capture-dir`, so the same helper drives
/// both the plain-echo and echo-plus-capture scenarios.
pub fn runner_command(scratch: &Scratch, bytes: u64, capture: bool) -> Command {
    let mut cmd = Command::new(runner_bin());
    cmd.arg("run")
        .arg("--jsonl")
        .arg(scratch.path("events.jsonl"));
    if capture {
        cmd.arg("--capture-dir").arg(scratch.path("capture"));
    }
    cmd.arg("--")
        .arg(emit_bin())
        .args(["--bytes", &bytes.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    cmd
}

/// Spawn `cmd` (stdout must already be `Stdio::piped()`), drain its stdout on a
/// background thread as fast as it arrives — exactly what a downstream log
/// consumer (or this bench's own drain of the *runner's* echoed stdout) would
/// do — wait for the child, and assert it exited cleanly. Draining is
/// mandatory: an undrained multi-MiB write blocks on the OS pipe buffer long
/// before the child (or the runner echoing it) can finish.
pub fn run_and_drain(mut cmd: Command) {
    let mut child = cmd.spawn().expect("spawn bench child");
    let mut stdout = child.stdout.take().expect("child stdout is piped");
    let drain = thread::spawn(move || {
        let mut sink = io::sink();
        io::copy(&mut stdout, &mut sink).expect("drain bench child stdout")
    });
    let status = child.wait().expect("wait for bench child");
    drain.join().expect("drain thread panicked");
    assert!(status.success(), "bench child exited with {status:?}");
}

/// Run `processkit-cli run --jsonl <scratch> -- bench_emit --bytes 0` once and
/// return the host-observed latency from just before `spawn` to the `time`
/// timestamp the runner itself stamped on its `run_started` JSONL event — the
/// "call to `run_started`" latency the startup-latency benchmark reports.
/// `bench_emit --bytes 0` writes nothing and exits immediately, so the
/// measured run's own lifetime past `run_started` stays negligible next to the
/// spawn/container-creation cost being measured.
pub fn measure_startup_latency(scratch: &Scratch) -> Duration {
    let jsonl = scratch.path("events.jsonl");
    let invocation = millis_since_epoch(SystemTime::now());
    let status = Command::new(runner_bin())
        .arg("run")
        .arg("--jsonl")
        .arg(&jsonl)
        .arg("--")
        .arg(emit_bin())
        .args(["--bytes", "0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .expect("spawn runner for startup-latency measurement");
    assert!(status.success(), "runner exited with {status:?}");

    let text = std::fs::read_to_string(&jsonl).unwrap_or_else(|err| {
        panic!("read events file {}: {err}", jsonl.display());
    });
    let run_started_time = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid JSON line"))
        .find(|event| event["event"] == "run_started")
        .and_then(|event| event["time"].as_str().map(str::to_string))
        .expect("a run_started event with a time field");

    let event_millis = parse_rfc3339_millis(&run_started_time);
    // Millisecond-precision clocks and the two processes' independent reads of
    // "now" can make the event timestamp round down to at or just before the
    // host's own `invocation` read; clamp at zero rather than reporting a
    // nonsensical negative latency.
    let latency_millis = event_millis.saturating_sub(invocation).max(0);
    Duration::from_millis(latency_millis as u64)
}

/// Milliseconds since the Unix epoch, matching the precision (and the same
/// `SystemTime`-since-`UNIX_EPOCH` basis) `src/events.rs`'s
/// `format_rfc3339_utc` stamps every JSONL event with.
fn millis_since_epoch(t: SystemTime) -> i64 {
    let dur = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    dur.as_millis() as i64
}

/// Parse an RFC 3339 UTC timestamp of the exact shape
/// `src/events.rs::format_rfc3339_utc` produces
/// (`YYYY-MM-DDTHH:MM:SS.sssZ`, millisecond precision, `Z` offset) into
/// milliseconds since the Unix epoch. A bench-local inverse of that module's
/// `civil_from_days` (Howard Hinnant's `days_from_civil`,
/// <http://howardhinnant.github.io/date_algorithms.html>) — duplicated here
/// rather than exposed from the library, since it is a generic calendar
/// utility with no connection to the runner's own primitives (unlike the hash/
/// capture/hint-classifier internals the other benches reach directly).
fn parse_rfc3339_millis(s: &str) -> i64 {
    let year: i64 = s[0..4].parse().expect("4-digit year");
    let month: u32 = s[5..7].parse().expect("2-digit month");
    let day: u32 = s[8..10].parse().expect("2-digit day");
    let hour: i64 = s[11..13].parse().expect("2-digit hour");
    let minute: i64 = s[14..16].parse().expect("2-digit minute");
    let second: i64 = s[17..19].parse().expect("2-digit second");
    let millis: i64 = s[20..23].parse().expect("3-digit millisecond");

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    secs * 1_000 + millis
}

/// The inverse of `civil_from_days`: a `(year, month, day)` civil date to a
/// count of days since the Unix epoch (1970-01-01), valid across the full
/// proleptic Gregorian range. Verified against the same known vectors
/// `src/events.rs::tests::timestamp_matches_known_vectors` pins for the
/// forward direction (see this crate's `T-187` task notes).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = i64::from(if month > 2 { month - 3 } else { month + 9 }); // [0, 11]
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Assert the inverse-calendar arithmetic above against the same known
/// vectors `src/events.rs::tests::timestamp_matches_known_vectors` pins for
/// the forward direction. Bench targets run with `harness = false` (see
/// `Cargo.toml`) — criterion supplies its own `main`, so a plain `cargo test`
/// never reaches a `#[test]` placed in a bench crate. `startup_latency_bench.rs`
/// calls this once, unconditionally, before it starts timing, so a broken
/// inverse fails loudly (a panic) instead of quietly reporting a bogus
/// latency number.
pub fn self_check_calendar_math() {
    assert_eq!(parse_rfc3339_millis("1970-01-01T00:00:00.000Z"), 0);
    assert_eq!(parse_rfc3339_millis("1970-01-02T00:00:00.000Z"), 86_400_000);
    assert_eq!(
        parse_rfc3339_millis("2001-09-09T01:46:40.000Z"),
        1_000_000_000_000
    );
    assert_eq!(
        parse_rfc3339_millis("2021-01-01T00:00:00.000Z"),
        1_609_459_200_000
    );
    assert_eq!(
        parse_rfc3339_millis("2021-01-01T00:00:00.123Z"),
        1_609_459_200_123
    );
}
