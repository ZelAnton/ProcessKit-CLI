//! Golden schema tests for the CLI's **non-event** machine-readable outputs:
//! `probe --json`, `list --json`, `inspect --json` (single and `--all`), the
//! `cancel`/`kill` ack and its `--all` report array, `prune --json` (with and
//! without `--dry-run`), `wait --report-outcome` (single and `--all`),
//! `attest --json`, `doctor --json`, and the `--error-format json` failure envelope
//! — the one family
//! whose channel is **stderr** rather than stdout, because that is precisely where
//! it keeps stdout reserved for successful output.
//!
//! These are the counterpart of what `tests/events.rs` does for the JSONL
//! lifecycle stream and `tests/cli_help.rs` does for the help surface: every case
//! drives the **built binary** (`AGENTS.md`, "Testing tiers"), validates what it
//! actually printed against the published JSON Schema document for that output
//! family (`fixtures/schema/cli/*.schema.json`), and pins it against a golden
//! fixture (`fixtures/schema/cli/*.jsonl`).
//!
//! Each family's test does four things, in this order:
//!
//! 1. run the binary and parse its stdout as one JSON value per line;
//! 2. validate that **live, un-normalized** output against the family's schema —
//!    this is the drift guard proper, since every document lists all its fields in
//!    `required` and sets `additionalProperties: false`, so a field added to or
//!    removed from a Rust struct fails here until the schema is published with it;
//!    then
//! 3. normalize the output (see [`normalize`]) and compare it byte-for-byte with
//!    the committed fixture — an intentional shape change is reviewed by
//!    regenerating with `UPDATE_MACHINE_SCHEMA_GOLDEN=1` and inspecting the diff,
//!    exactly as `tests/cli_help.rs` and the JSONL golden stream are regenerated;
//!    and finally
//! 4. read that fixture back off disk and validate it against the schema too, so
//!    the published example can never go stale or be hand-edited into something
//!    the document rejects.
//!
//! See `fixtures/schema/cli/README.md` for the layout, the versioning decision
//! these outputs embody (five of these nine families — `probe`, `inspect`,
//! `attest`, `doctor`, and the error envelope — carry their own `probe_version` /
//! `snapshot_version` / `attestation_version` / `doctor_version` / `error_version`;
//! the other four deliberately carry no version field), and what the normalization
//! does and does not touch.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};

use common::{bin, command_with_flags, scratch, shell_inline};
use jsonschema::Validator;
use serde_json::{Map, Value, json};

use processkit_cli::registry::test_support::{write_stale_entry, write_unprobeable_entry};

/// Set this when running the tests to rewrite every golden fixture from the
/// binary's current output instead of comparing against it.
const UPDATE_ENV: &str = "UPDATE_MACHINE_SCHEMA_GOLDEN";

/// The output families published under `fixtures/schema/cli/`, each with a
/// `<family>.schema.json` document and a `<family>.jsonl` fixture.
const FAMILIES: &[&str] = &[
    "probe",
    "list",
    "inspect",
    "control-ack",
    "prune",
    "wait",
    "error",
    "attest",
    "doctor",
];

// ---------------------------------------------------------------------------
// Fixed sample values.
//
// A golden fixture has to be stable across two runs of the same test and across
// every platform in CI, so values that legitimately vary — timestamps, PIDs,
// absolute paths, the control endpoint, the argv fingerprint, the containment
// mechanism, the live member list, this build's own version, and the CLI surface
// token list — are replaced by these fixed samples before the fixture is written
// or compared. This is the same convention `fixtures/schema/v1/events.jsonl`
// documents for its own timestamps, run id, and PIDs. Shapes (which fields exist,
// their types, whether they are null) are never normalized: those are exactly
// what the fixture pins.
// ---------------------------------------------------------------------------

const SAMPLE_STARTED_AT: &str = "2026-07-20T21:00:00.000Z";
/// The instant an attestation was decided — a *different* fact from a run's start
/// time, and given its own sample (later than [`SAMPLE_STARTED_AT`]) so the fixture
/// shows a verdict taken during a run rather than at its start.
const SAMPLE_CHECKED_AT: &str = "2026-07-20T21:00:05.000Z";
const SAMPLE_ROOT_PID: u32 = 4242;
/// The attesting client's own pid, distinct from [`SAMPLE_ROOT_PID`]: the process
/// asking is not in general the run's root child, and a shared sample would blur the
/// one field whose whole point is that it names the *caller*.
const SAMPLE_PEER_PID: u32 = 4343;
const SAMPLE_JSONL: &str = "/samples/build-42.jsonl";
const SAMPLE_CAPTURE_DIR: &str = "/samples/build-42/capture";
const SAMPLE_ENDPOINT: &str = "/samples/pkc-0123456789abcdef/c.sock";
const SAMPLE_SOCKET_DIR: &str = "/samples/pkc-0123456789abcdef";
const SAMPLE_ARGV_SHA256: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const SAMPLE_VERSION: &str = "0.0.0";
// Use the newly published mechanism in the stable sample so every machine-output
// family proves its schema and fixture accept the FreeBSD projection, even when the
// test host is Windows or Linux.
const SAMPLE_MECHANISM: &str = "process_reaper";

// ---------------------------------------------------------------------------
// `doctor --json`'s samples.
//
// A qualification report is the one family here whose content is *about the host
// that produced it*, so almost every value in it legitimately differs between two
// platforms — and, for the timings, between two runs on the same one.
//
// The samples below are therefore per-field fixed values, not a portrait of one
// plausible machine: two of the fields this family carries reuse samples the other
// families already fixed (`mechanism`'s `process_reaper` and `endpoint`'s unix socket
// path), which no single host would report together. That is deliberate — sharing a
// sample keeps one normalizer entry per field name across every family, and the
// fixture's job is to pin **shapes**: which facts are present, which are null, which
// are counted. What each fact actually says on a given platform is asserted by
// `tests/doctor.rs`, which runs on that platform and can therefore be specific about
// it.
// ---------------------------------------------------------------------------

const SAMPLE_OS: &str = "windows";
const SAMPLE_REGISTRY_DIR: &str = "/samples/registry";
const SAMPLE_PROTECTION: &str = "windows_owner_only_dacl";
const SAMPLE_ABRUPT_CLEANUP: &str = "whole_tree";
const SAMPLE_TRANSPORT: &str = "windows_named_pipe";
/// Every `elapsed_ms`, top-level and per-phase: a wall-clock measurement is
/// different on every single run, so pinning one would make this fixture fail at
/// random rather than on a change.
const SAMPLE_ELAPSED_MS: u64 = 12;
/// The observed member counts a healthy qualification reports before teardown
/// (`inspected_members`, `members_before`). How many processes a platform's member
/// enumeration reports for the same two-process scratch tree is a property of that
/// platform's containment mechanism.
const SAMPLE_MEMBER_COUNT: u64 = 1;
/// The post-teardown count (`remaining`). Its own sample rather than
/// [`SAMPLE_MEMBER_COUNT`] so the sample host stays coherent — one member contained,
/// none left — and because what a platform's post-kill snapshot can report differs
/// from what its live one does: on the POSIX `process_group` fallback that snapshot
/// still lists a just-exited child nobody has reaped (`docs/schema.md`,
/// "cleanup_finished"), which is exactly what `teardown_snapshot_conclusive` is for.
const SAMPLE_REMAINING: u64 = 0;
/// The directory a failed qualification keeps its evidence in — an absolute path on
/// the machine that ran the test.
const SAMPLE_DIAGNOSTICS_DIR: &str = "/samples/doctor-diagnostics";

/// The failure envelope's `message` is the one field this directory publishes that
/// is **deliberately not part of its contract** (`fixtures/schema/cli/error.schema.json`):
/// it may be reworded in any release, so pinning its prose in a golden would turn
/// every clarification into a fixture conflict while guarding nothing a consumer is
/// allowed to depend on. Replacing it with a fixed sample is how that decision is
/// *stated* rather than merely intended — the fixture pins the envelope's shape, its
/// `code`, `kind`, `operation`, `run_id`, and `retryable`, and says out loud that the
/// prose is free. The live, real message is still validated against the schema (as a
/// string that must be present), like every other field.
const SAMPLE_MESSAGE: &str = "<free-text explanation, not part of the contract>";

/// The `surface` token list is truncated to this fixed sample on purpose: the
/// real list is already pinned exhaustively by the `fixtures/cli-help/` golden
/// snapshots and by `probe`'s own unit test, so repeating it here would make
/// every new flag churn this fixture without guarding anything new. The live
/// report's complete, real surface is still validated against the schema.
fn sample_surface() -> Value {
    json!(["run", "run:--jsonl"])
}

/// `doctor`'s two diagnostic arrays, with their length preserved and their text
/// replaced: an empty array stays empty (the schema conditions on that), and a
/// non-empty one keeps its element count so a report naming two unmet requirements
/// is still distinguishable from one naming a single reason.
fn sample_reasons(value: &Value) -> Value {
    let count = value.as_array().map_or(0, Vec::len);
    Value::Array(
        (0..count)
            .map(|_| json!("<free-text reason, not part of the contract>"))
            .collect(),
    )
}

/// A live container's member list is genuinely variable (how many processes a
/// shell fixture spawns differs per platform), so the fixture carries one fixed
/// sample member instead. The real member list is still schema-validated.
fn sample_members() -> Value {
    json!([{
        "pid": SAMPLE_ROOT_PID,
        "ppid": 4200,
        "name": "child",
        "start_time": "133456789000000000"
    }])
}

// ---------------------------------------------------------------------------
// Schema documents, fixtures, and the normalizer.
// ---------------------------------------------------------------------------

/// The published directory holding both halves of every family's contract.
fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/schema/cli")
}

fn schema_path(family: &str) -> PathBuf {
    schema_dir().join(format!("{family}.schema.json"))
}

fn fixture_path(family: &str) -> PathBuf {
    schema_dir().join(format!("{family}.jsonl"))
}

/// Compile a family's schema document. Each is small and self-contained, so a
/// per-call compile costs nothing worth caching.
fn schema_validator(family: &str) -> Validator {
    let path = schema_path(family);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read schema {}: {err}", path.display()));
    let schema: Value = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{} is valid JSON: {err}", path.display()));
    jsonschema::validator_for(&schema)
        .unwrap_or_else(|err| panic!("{} is a valid JSON Schema (2020-12): {err}", path.display()))
}

/// Assert every value in `values` validates against `family`'s schema document,
/// collecting every violation first so one failure reports all offending lines
/// rather than only the first (the same shape `tests/events.rs` uses).
fn assert_validates(family: &str, what: &str, values: &[Value]) {
    let validator = schema_validator(family);
    let failures: Vec<String> = values
        .iter()
        .enumerate()
        .filter_map(|(i, value)| {
            let errors: Vec<String> = validator
                .iter_errors(value)
                .map(|e| e.to_string())
                .collect();
            (!errors.is_empty())
                .then(|| format!("line {}: {value}\n    {}", i + 1, errors.join("\n    ")))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{what} did not validate against {}:\n{}",
        schema_path(family).display(),
        failures.join("\n")
    );
}

/// Rebuild `value` with every object's keys in sorted order and every volatile
/// value replaced by its fixed sample (see the constants above). Recursive, and
/// deliberately keyed on **field names** rather than value patterns, so it is
/// obvious from this one function what a fixture line does and does not pin.
///
/// `family` scopes the field names that belong to exactly one output family. Two
/// families now spell a field the same way and mean something with different
/// stability: `probe --json`'s `mismatches` names only what the *caller asked for*
/// (fixed text, pinned verbatim), while `doctor --json`'s also names what the *host
/// answered* ("this host selected `job_object`"), which no two platforms agree on.
/// The key alone stopped being enough to decide, so the family decides.
fn normalize(family: &str, value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), normalize_field(family, key, &map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| normalize(family, item)).collect())
        }
        other => other.clone(),
    }
}

/// The per-field half of [`normalize`]. A `null` is always left alone:
/// nullability is part of the shape a fixture pins, never something the
/// normalizer gets to decide.
fn normalize_field(family: &str, key: &str, value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    // `doctor --json` is a report *about the host that produced it*, so its own
    // fields are scoped to it rather than shared: every one of them is volatile in a
    // way the other families' same-named fields need not be, and one of them
    // (`mismatches`) is a name another family already pins verbatim.
    if family == "doctor"
        && let Some(sample) = normalize_doctor_field(key, value)
    {
        return sample;
    }
    match key {
        "started_at" => json!(SAMPLE_STARTED_AT),
        "checked_at" => json!(SAMPLE_CHECKED_AT),
        "root_pid" => json!(SAMPLE_ROOT_PID),
        "peer_pid" => json!(SAMPLE_PEER_PID),
        "jsonl" => json!(SAMPLE_JSONL),
        "capture_dir" => json!(SAMPLE_CAPTURE_DIR),
        "endpoint" => json!(SAMPLE_ENDPOINT),
        "socket_dir" => json!(SAMPLE_SOCKET_DIR),
        "argv_sha256" => json!(SAMPLE_ARGV_SHA256),
        "version" => json!(SAMPLE_VERSION),
        "mechanism" => json!(SAMPLE_MECHANISM),
        // Only the failure envelope has a `message`; no other family published here
        // carries a field by that name (the JSONL event stream's own `message` lives
        // in `fixtures/schema/v1/` and is pinned by `tests/events.rs`, not here).
        "message" => json!(SAMPLE_MESSAGE),
        "surface" => sample_surface(),
        "members" => sample_members(),
        _ => normalize(family, value),
    }
}

/// The `doctor --json`-only fields, or `None` for a key this family does not own
/// (which then falls through to the shared table above — `version`, and the
/// `mechanism` and `endpoint` it shares with the control-plane families).
///
/// `remaining_pids` is normalized to the empty array its healthy case carries rather
/// than to a sample pid: the field's shape is what this fixture pins, and a pinned pid
/// would only invite it to be read as a promise about the count.
fn normalize_doctor_field(key: &str, value: &Value) -> Option<Value> {
    Some(match key {
        "os" => json!(SAMPLE_OS),
        "dir" => json!(SAMPLE_REGISTRY_DIR),
        "protection" => json!(SAMPLE_PROTECTION),
        "abrupt_cleanup" => json!(SAMPLE_ABRUPT_CLEANUP),
        "transport" => json!(SAMPLE_TRANSPORT),
        "elapsed_ms" => json!(SAMPLE_ELAPSED_MS),
        "inspected_members" | "members_before" => json!(SAMPLE_MEMBER_COUNT),
        "remaining" => json!(SAMPLE_REMAINING),
        "remaining_pids" => json!([]),
        "confirmed_empty" | "teardown_snapshot_conclusive" => json!(true),
        // Both diagnostic arrays name values read off the host that produced the
        // report ("this host selected `process_reaper`"), so their *text* is as
        // platform-bound as the facts it quotes. Emptiness is preserved because that
        // is shape — the schema conditions on it — while the strings themselves are
        // replaced, for the same reason `error.jsonl`'s `message` is: they are
        // diagnostics, not a contract, and the live output is still schema-validated
        // with its real text in place.
        "failures" | "mismatches" => sample_reasons(value),
        "diagnostics_dir" => json!(SAMPLE_DIAGNOSTICS_DIR),
        _ => return None,
    })
}

/// Render fixture text: one canonical JSON value per line, LF-terminated.
fn render(values: &[Value]) -> String {
    let mut text = String::new();
    for value in values {
        text.push_str(&serde_json::to_string(value).expect("a parsed JSON value re-serializes"));
        text.push('\n');
    }
    text
}

/// The whole per-family contract check: validate the live output, pin its
/// normalized form against the committed fixture (rewriting it under
/// `UPDATE_MACHINE_SCHEMA_GOLDEN=1`), then validate that fixture in turn.
fn check_family(family: &str, live: &[Value]) {
    assert!(
        !live.is_empty(),
        "`{family}`: the binary printed no machine-readable output to capture"
    );
    assert_validates(family, &format!("`{family}`'s live output"), live);

    let normalized: Vec<Value> = live.iter().map(|value| normalize(family, value)).collect();
    let rendered = render(&normalized);
    let path = fixture_path(family);
    if std::env::var_os(UPDATE_ENV).is_some() {
        fs::write(&path, &rendered)
            .unwrap_or_else(|err| panic!("rewrite fixture {}: {err}", path.display()));
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "read fixture {}: {err}; regenerate with {UPDATE_ENV}=1",
            path.display()
        )
    });
    assert_eq!(
        rendered,
        expected.replace("\r\n", "\n"),
        "machine-output drift for `{family}`; if intentional, regenerate with \
         {UPDATE_ENV}=1 and review the fixture diff alongside {}",
        schema_path(family).display()
    );

    let fixture_lines: Vec<Value> = expected
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|err| {
                panic!("`{family}` fixture line is valid JSON: {line}: {err}")
            })
        })
        .collect();
    assert_validates(
        family,
        &format!("`{family}`'s golden fixture"),
        &fixture_lines,
    );
}

// ---------------------------------------------------------------------------
// Driving the binary.
// ---------------------------------------------------------------------------

/// The registry directory a scenario points every command at, kept apart from
/// the scratch directory's own `--jsonl` file so a scan never trips over it.
fn registry_dir(scratch_dir: &Path) -> PathBuf {
    scratch_dir.join("registry")
}

/// Invoke the binary against an isolated scratch registry and wait for it.
fn cli(registry: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .unwrap_or_else(|err| panic!("spawn `processkit-cli {}`: {err}", args.join(" ")))
}

/// Assert the command exited with `code` and parse its stdout as one JSON value
/// per non-empty line — the shape every output family here uses.
fn machine_output(out: &Output, code: i32, what: &str) -> Vec<Value> {
    assert_eq!(
        out.status.code(),
        Some(code),
        "{what} must exit {code}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout.clone())
        .unwrap_or_else(|err| panic!("{what} prints UTF-8: {err}"));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("{what} prints valid JSON: {line}: {err}"))
        })
        .collect()
}

/// Assert the command exited with `code` and parse its **stderr** as one JSON value
/// per non-empty line — the failure envelope's channel.
///
/// The counterpart of [`machine_output`] for the one family that is deliberately not
/// on stdout, and it asserts that invariant rather than assuming it: stdout must
/// carry no envelope, because a failure must never contaminate the successful output
/// a caller may already be parsing (`fixtures/schema/cli/error.schema.json`).
fn machine_stderr(out: &Output, code: i32, what: &str) -> Vec<Value> {
    assert_eq!(
        out.status.code(),
        Some(code),
        "{what} must exit {code}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("error_version"),
        "{what} must keep its envelope off stdout: {stdout}"
    );
    let text = String::from_utf8(out.stderr.clone())
        .unwrap_or_else(|err| panic!("{what} is UTF-8: {err}"));
    let lines: Vec<Value> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("{what} prints valid JSON on stderr: {line}: {err}"))
        })
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "{what} prints exactly one envelope, nothing else: {text}"
    );
    lines
}

/// Poll `cond` until it holds or `timeout` elapses (then panic).
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

/// A child that outlives any of these scenarios; the run's own `--timeout` ends
/// it if a test panics before tearing it down.
fn long_child() -> Vec<String> {
    if cfg!(windows) {
        shell_inline("ping -n 300 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 300")
    }
}

/// A child that exits cleanly after ~15s — brief only next to [`long_child`], and
/// deliberately the same width as `tests/registry.rs`'s `inspectable_child`, for the
/// same measured reason.
///
/// The window has to cover *two* processes in sequence before the run ends: the
/// runner publishing its record, and a separate `wait --report-outcome` client being
/// spawned and reaching its first probe while the run is still live. Only a waiter
/// that observed the run live reports the terminal outcome; one that arrives late is
/// honestly `unknown` (`src/wait.rs`), and this file's scenario hard-asserts the
/// `reported` line *and* pins it in `fixtures/schema/cli/wait.jsonl`, so missing the
/// window fails both the assertion and the byte-for-byte fixture comparison. ~5s is
/// the width `inspectable_child` started at: enough on an idle machine, but it flaked
/// reproducibly under host contention (see its own comment), and the race here is the
/// same one with an extra client spawn inside the window. Widening costs wall-clock
/// time and nothing else: the scenario waits for the run to end either way.
fn brief_child() -> Vec<String> {
    if cfg!(windows) {
        shell_inline("ping -n 16 127.0.0.1 >nul")
    } else {
        shell_inline("sleep 15")
    }
}

/// Whether `registry` holds a record for `run_id` that has already published its
/// control endpoint. Waiting for *that*, rather than for the record's mere
/// existence, keeps `endpoint`'s nullness out of the fixture's race window.
fn record_is_published(registry: &Path, run_id: &str) -> bool {
    let Ok(read_dir) = fs::read_dir(registry) else {
        return false;
    };
    read_dir.filter_map(Result::ok).any(|entry| {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            return false;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            return false;
        };
        let Ok(record) = serde_json::from_str::<Value>(&text) else {
            return false;
        };
        record["run_id"] == json!(run_id) && record["endpoint"].is_string()
    })
}

/// Start a live run under `run_id` and return once its record is published. The
/// run carries its own `--timeout` so a panicking test cannot leave it behind.
fn spawn_live_run(dir: &Path, registry: &Path, run_id: &str, extra: &[&str]) -> Child {
    let mut flags = vec!["--run-id", run_id, "--timeout", "60s"];
    flags.extend_from_slice(extra);
    let child = command_with_flags(
        dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry)],
        &flags,
        long_child(),
    )
    .spawn()
    .unwrap_or_else(|err| panic!("spawn the runner for `{run_id}`: {err}"));
    wait_until(
        || record_is_published(registry, run_id),
        Duration::from_secs(20),
        &format!("the record for `{run_id}`"),
    );
    child
}

/// End a live run the way an operator would — through the control plane — and
/// reap the runner process.
fn cancel_run(registry: &Path, run_id: &str, mut child: Child) {
    let out = cli(registry, &["cancel", "--run-id", run_id]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "cancelling `{run_id}` succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = child.wait();
}

/// Write a `.lock` file with no `.json` sibling and backdate it well past the
/// registry's orphan-lock age floor, so `prune` treats it as a confirmed orphan
/// rather than a reservation still being written (`src/registry/mod.rs`,
/// `ORPHAN_LOCK_MIN_AGE`).
fn write_aged_orphan_lock(registry: &Path, file_name: &str) {
    let path = registry.join(file_name);
    fs::write(&path, b"").expect("write the orphaned lock fixture");
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open the orphaned lock fixture to backdate it");
    file.set_modified(SystemTime::now() - Duration::from_secs(600))
        .expect("backdate the orphaned lock fixture");
}

// ---------------------------------------------------------------------------
// The families.
// ---------------------------------------------------------------------------

/// Every published document is a self-contained draft 2020-12 schema — the
/// convention that lets the `jsonschema` dev-dependency stay
/// `default-features = false` (no remote-`$ref` resolver, no `reqwest`), the same
/// way `fixtures/schema/v1/schema.json` does.
#[test]
fn every_schema_document_is_self_contained_draft_2020_12() {
    for family in FAMILIES {
        let path = schema_path(family);
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read schema {}: {err}", path.display()));
        assert!(
            !text.contains("\"$ref\": \"http"),
            "{} must resolve every $ref internally (no remote reference)",
            path.display()
        );
        let schema: Value = serde_json::from_str(&text)
            .unwrap_or_else(|err| panic!("{} is valid JSON: {err}", path.display()));
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema",
            "{} declares draft 2020-12",
            path.display()
        );
        // Compiling it is the real check that every internal `$ref` resolves.
        let _ = schema_validator(family);
    }
}

/// `probe --json`: the healthy self-report, plus the fail-closed incompatible
/// report an unmet `--require-*` expectation produces (exit `110`, `compatible:
/// false`, and the concrete `mismatches`). Both go through the same document.
#[test]
fn probe_report_matches_its_schema_and_fixture() {
    let compatible = Command::new(bin())
        .args(["probe", "--json"])
        .output()
        .expect("spawn the probe client");
    let mut lines = machine_output(&compatible, 0, "`probe --json`");

    let incompatible = Command::new(bin())
        .args([
            "probe",
            "--json",
            "--require-surface",
            "run:--not-a-real-flag",
        ])
        .output()
        .expect("spawn the probe client");
    lines.extend(machine_output(
        &incompatible,
        110,
        "`probe --json` with an unmet expectation",
    ));

    assert_eq!(lines[0]["compatible"], json!(true));
    assert_eq!(lines[1]["compatible"], json!(false));
    check_family("probe", &lines);
}

/// `list --json`: one object per registry entry, one per line, covering all three
/// health values — a live run publishing every optional field, a confirmed-stale
/// leftover publishing none of them, and an entry whose liveness could not be
/// probed at all.
#[test]
fn list_entries_match_their_schema_and_fixture() {
    let dir = scratch("machine-list");
    let registry = registry_dir(&dir);
    let capture = dir.join("capture");
    let capture_arg = capture.to_string_lossy().into_owned();

    let child = spawn_live_run(
        &dir,
        &registry,
        "build-42",
        &[
            "--capture-dir",
            &capture_arg,
            "--label",
            "pipeline=ci",
            "--label",
            "lane=build",
        ],
    );
    // Written after the live run so the endpoint wait above reads only its record.
    write_stale_entry(&registry, "build-43", "build-43");
    write_unprobeable_entry(&registry, "build-44", "build-44");

    let out = cli(&registry, &["list", "--json"]);
    let lines = machine_output(&out, 0, "`list --json`");
    assert_eq!(
        lines.len(),
        3,
        "one line per registry entry, ordered by run_id: {lines:?}"
    );
    assert_eq!(lines[0]["health"], json!("live"));
    assert_eq!(lines[1]["health"], json!("stale"));
    assert_eq!(lines[2]["health"], json!("unprobed"));

    check_family("list", &lines);

    cancel_run(&registry, "build-42", child);
    let _ = fs::remove_dir_all(&dir);
}

/// `inspect --json`: the single-run snapshot, then the `--all` array whose one
/// entry carries that same snapshot inline with a null `error`.
#[test]
fn inspect_snapshots_match_their_schema_and_fixture() {
    let dir = scratch("machine-inspect");
    let registry = registry_dir(&dir);
    let capture = dir.join("capture");
    let capture_arg = capture.to_string_lossy().into_owned();

    let child = spawn_live_run(
        &dir,
        &registry,
        "build-42",
        &["--capture-dir", &capture_arg],
    );

    let single = cli(&registry, &["inspect", "--run-id", "build-42", "--json"]);
    let mut lines = machine_output(&single, 0, "`inspect --run-id … --json`");
    assert_eq!(lines[0]["run_id"], json!("build-42"));
    assert!(
        lines[0]["members"]
            .as_array()
            .is_some_and(|members| !members.is_empty()),
        "a live run's snapshot lists its container members: {}",
        lines[0]
    );

    let all = cli(&registry, &["inspect", "--all", "--json"]);
    let all_lines = machine_output(&all, 0, "`inspect --all --json`");
    assert_eq!(all_lines.len(), 1, "the report is one JSON array line");
    assert_eq!(
        all_lines[0][0]["status"],
        json!("inspected"),
        "the only live run inspects successfully: {}",
        all_lines[0]
    );
    lines.extend(all_lines);

    check_family("inspect", &lines);

    cancel_run(&registry, "build-42", child);
    let _ = fs::remove_dir_all(&dir);
}

/// The mutating verbs' machine output: a `cancel` ack, a `kill` ack, and the
/// aggregate `cancel --all` report array.
#[test]
fn control_acks_match_their_schema_and_fixture() {
    let mut lines = Vec::new();

    for (verb, tag) in [
        ("cancel", "machine-ack-cancel"),
        ("kill", "machine-ack-kill"),
    ] {
        let dir = scratch(tag);
        let registry = registry_dir(&dir);
        let mut child = spawn_live_run(&dir, &registry, "build-42", &[]);
        let out = cli(&registry, &[verb, "--run-id", "build-42"]);
        let ack = machine_output(&out, 0, &format!("`{verb} --run-id …`"));
        assert_eq!(ack.len(), 1, "one ack line: {ack:?}");
        assert_eq!(ack[0]["action"], json!(verb));
        lines.extend(ack);
        let _ = child.wait();
        let _ = fs::remove_dir_all(&dir);
    }

    let dir = scratch("machine-ack-all");
    let registry = registry_dir(&dir);
    let mut child = spawn_live_run(&dir, &registry, "build-42", &[]);
    let out = cli(&registry, &["cancel", "--all"]);
    let report = machine_output(&out, 0, "`cancel --all`");
    assert_eq!(report.len(), 1, "the report is one JSON array line");
    assert_eq!(
        report[0][0]["status"],
        json!("accepted"),
        "the only live run accepts the cancel: {}",
        report[0]
    );
    lines.extend(report);
    let _ = child.wait();

    check_family("control-ack", &lines);
    let _ = fs::remove_dir_all(&dir);
}

/// `prune --json`: the plain tally, then the `--dry-run` preview of the same
/// registry state with its `candidates` list. The state deliberately exercises
/// every tally field at once — one reapable stale entry, one live run left
/// untouched, one entry whose liveness could not be probed, and one aged orphan
/// lock file.
#[test]
fn prune_reports_match_their_schema_and_fixture() {
    let dir = scratch("machine-prune");
    let registry = registry_dir(&dir);

    let child = spawn_live_run(&dir, &registry, "build-42", &[]);
    write_stale_entry(&registry, "build-43", "build-43");
    write_unprobeable_entry(&registry, "build-44", "build-44");
    write_aged_orphan_lock(&registry, "build-45.lock");

    // The preview runs first: it is non-destructive, so the real prune below sees
    // exactly the state it previewed.
    let preview = cli(&registry, &["prune", "--dry-run", "--json"]);
    let preview_lines = machine_output(&preview, 0, "`prune --dry-run --json`");
    assert_eq!(preview_lines.len(), 1, "one JSON object: {preview_lines:?}");

    let real = cli(&registry, &["prune", "--json"]);
    let mut lines = machine_output(&real, 0, "`prune --json`");
    assert_eq!(lines.len(), 1, "one JSON object: {lines:?}");
    assert_eq!(
        lines[0],
        json!({"pruned": 1, "live": 1, "unprobed": 1, "orphaned_locks": 1}),
        "the tally counts the stale entry, the live run, the unprobeable entry, \
         and the orphaned lock separately"
    );
    assert_eq!(
        preview_lines[0]["pruned"], lines[0]["pruned"],
        "the preview predicted exactly what the real pass reaped"
    );

    // Fixture order follows the schema's root `oneOf`: the tally, then the
    // dry-run report.
    lines.extend(preview_lines);
    let candidates = lines[1]["candidates"].as_array().unwrap_or_else(|| {
        panic!(
            "the dry-run report carries a candidates array: {}",
            lines[1]
        )
    });
    assert_eq!(candidates.len(), 2, "one entry and one orphaned lock");
    assert_eq!(candidates[0]["kind"], json!("entry"));
    assert_eq!(candidates[1]["kind"], json!("orphaned_lock"));

    check_family("prune", &lines);

    cancel_run(&registry, "build-42", child);
    let _ = fs::remove_dir_all(&dir);
}

/// `--error-format json`: the bounded failure envelope, in the variants worth
/// pinning — a run id that is named versus one that cannot be (`null`), a retryable
/// verdict versus a final one, and every reserved code that belongs to a single
/// subcommand's own verdict (110, 112, 114, 115, 116) alongside the shared
/// `CONTROL` (103).
///
/// That set is not maintained by hand: `src/error_envelope.rs`'s
/// `every_kind_with_a_code_of_its_own_has_a_line_in_the_golden_fixture` derives it
/// from this build's own code-to-kind table and fails if one of those kinds has no
/// line in `error.jsonl`, so minting a verdict code without adding a scenario here
/// is a test failure rather than a quiet gap in what this fixture publishes.
///
/// The scenarios are deliberately the cheap, deterministic ones wherever a fact
/// allows it — no live run is needed to fail a lookup, and `wait`'s own deadline
/// runs out in 150ms against the very entry whose liveness nothing can establish —
/// and several of them also pin the invariant that gives this family its own
/// channel: `probe --json` prints its full report to stdout *and* exits 110,
/// `events --validate` prints its human summary to stdout *and* exits 114,
/// `attest --json` prints its attestation *and* exits 115, and `doctor --json`
/// prints its qualification report *and* exits 116 — in every case the envelope is
/// on stderr and stdout is exactly what it always was. The kinds not pinned here are
/// covered by `tests/error_envelope.rs`, which drives the remaining taxonomy against
/// the live binary without re-pinning the shape.
#[test]
fn error_envelopes_match_their_schema_and_fixture() {
    let dir = scratch("machine-error");
    let registry = registry_dir(&dir);
    // A confirmed-stale entry and an unprobeable one: the two registry states whose
    // difference this envelope exists to make machine-readable.
    write_stale_entry(&registry, "build-43", "build-43");
    write_unprobeable_entry(&registry, "build-44", "build-44");

    // Nothing names `build-99` at all — not the same fact as either state above.
    let missing = cli(
        &registry,
        &["--error-format", "json", "inspect", "--run-id", "build-99"],
    );
    let mut lines = machine_stderr(&missing, 103, "`inspect` against an unregistered run");
    assert_eq!(lines[0]["kind"], json!("not_found"));
    assert_eq!(lines[0]["run_id"], json!("build-99"));
    assert!(
        missing.stdout.is_empty(),
        "a failed inspect prints nothing at all on stdout: {}",
        String::from_utf8_lossy(&missing.stdout)
    );

    // The runner is *confirmed* gone. Flag after the subcommand on purpose: the
    // position must not matter, and the fixture is generated by whichever spelling
    // this test uses.
    let stale = cli(
        &registry,
        &["inspect", "--run-id", "build-43", "--error-format", "json"],
    );
    let stale_lines = machine_stderr(&stale, 103, "`inspect` against a confirmed-stale entry");
    assert_eq!(stale_lines[0]["kind"], json!("stale"));
    assert_eq!(stale_lines[0]["code"], json!(103));
    assert_eq!(
        stale_lines[0]["retryable"],
        json!(false),
        "a gone runner does not come back on a retry"
    );
    lines.extend(stale_lines);

    // Nothing at all was established about this one — the single retryable member of
    // the CONTROL family, and the distinction that would be invisible in the exit
    // code alone.
    let unprobed = cli(
        &registry,
        &["inspect", "--run-id", "build-44", "--error-format", "json"],
    );
    let unprobed_lines = machine_stderr(&unprobed, 103, "`inspect` against an unprobeable entry");
    assert_eq!(unprobed_lines[0]["kind"], json!("unprobed"));
    assert_eq!(
        unprobed_lines[0]["retryable"],
        json!(true),
        "an unprobeable entry is the one CONTROL failure a second probe may settle"
    );
    assert_eq!(
        unprobed_lines[0]["code"],
        json!(103),
        "the three kinds above all refine the one exit code a shell would have seen"
    );
    lines.extend(unprobed_lines);

    // A failure with no run to name, and with a full machine-readable report of its
    // own already on stdout.
    let probe = cli(
        &registry,
        &[
            "probe",
            "--json",
            "--require-surface",
            "run:--not-a-real-flag",
            "--error-format",
            "json",
        ],
    );
    let probe_lines = machine_stderr(&probe, 110, "`probe --json` with an unmet expectation");
    assert_eq!(probe_lines[0]["kind"], json!("probe_incompatible"));
    assert_eq!(probe_lines[0]["run_id"], Value::Null);
    let report: Value = serde_json::from_str(String::from_utf8_lossy(&probe.stdout).trim())
        .expect("the probe report is still valid JSON on stdout");
    assert_eq!(
        report["compatible"],
        json!(false),
        "stdout still carries the probe's own report, unchanged: {report}"
    );
    lines.extend(probe_lines);

    // A deadline of the *caller's* own, against the same entry `inspect` just refused
    // to act on: nothing can establish that run's liveness, so `wait` never reads it
    // as finished and runs out its own window. The verdict is emphatically not the
    // run's `timeout` (106) — the run was never touched — and it is the second
    // retryable line here for a different reason than `unprobed`'s: waiting again is
    // the intended response, not a hope that a probe settles.
    let gave_up = cli(
        &registry,
        &[
            "wait",
            "--run-id",
            "build-44",
            "--timeout",
            "150ms",
            "--error-format",
            "json",
        ],
    );
    let gave_up_lines = machine_stderr(&gave_up, 112, "`wait` giving up on an unprobeable entry");
    assert_eq!(gave_up_lines[0]["kind"], json!("wait_timeout"));
    assert_eq!(gave_up_lines[0]["operation"], json!("wait"));
    assert_eq!(gave_up_lines[0]["run_id"], json!("build-44"));
    assert_eq!(
        gave_up_lines[0]["retryable"],
        json!(true),
        "the waiter gave up without touching anything, so waiting again is the intended response"
    );
    assert!(
        gave_up.stdout.is_empty(),
        "a plain `wait` prints nothing at all on stdout — its machine-readable form is \
         `--report-outcome`, a different family entirely: {}",
        String::from_utf8_lossy(&gave_up.stdout)
    );
    lines.extend(gave_up_lines);

    // A verdict about a document rather than a run: `events` never contacts a runner.
    let bad_stream = dir.join("not-conforming.jsonl");
    fs::write(
        &bad_stream,
        b"{\"schema_version\":1,\"event\":\"not_an_event\"}\n",
    )
    .expect("write the non-conforming stream fixture");
    let invalid = cli(
        &registry,
        &[
            "events",
            "--file",
            &bad_stream.to_string_lossy(),
            "--validate",
            "--error-format",
            "json",
        ],
    );
    let invalid_lines = machine_stderr(&invalid, 114, "`events --validate` on a bad stream");
    assert_eq!(invalid_lines[0]["kind"], json!("events_invalid"));
    assert_eq!(invalid_lines[0]["operation"], json!("events"));
    assert!(
        !invalid.stdout.is_empty(),
        "the human-readable conformance report still goes to stdout"
    );
    lines.extend(invalid_lines);

    // A *decided* verdict rather than a failure to reach anything — the one envelope
    // here whose code is not shared with any other kind, and the distinction
    // `not_a_member` exists to make: this test process really is outside the live
    // run it just asked about.
    let child = spawn_live_run(&dir, &registry, "build-42", &[]);
    let outsider = cli(
        &registry,
        &[
            "attest",
            "--run-id",
            "build-42",
            "--json",
            "--error-format",
            "json",
        ],
    );
    let outsider_lines = machine_stderr(&outsider, 115, "`attest` from outside the run");
    assert_eq!(outsider_lines[0]["kind"], json!("not_a_member"));
    assert_eq!(outsider_lines[0]["operation"], json!("attest"));
    assert_eq!(outsider_lines[0]["run_id"], json!("build-42"));
    assert_eq!(
        outsider_lines[0]["retryable"],
        json!(false),
        "asking again will not make the caller a member"
    );
    assert!(
        !outsider.stdout.is_empty(),
        "the attestation itself still goes to stdout — the envelope reports the verdict's \
         consequence, it does not replace the answer"
    );
    lines.extend(outsider_lines);
    cancel_run(&registry, "build-42", child);

    // The other decided verdict, and the one about the *machine* rather than about a
    // run or a document: `doctor` qualified this host and was told to require a
    // mechanism no platform reports. The only scenario here that is genuinely
    // side-effecting — it drives a real scratch run — which is the price of the fact
    // it pins: the code no other kind carries (116), an `operation` of `doctor`, and a
    // `run_id` that is null because the only run involved is the one `doctor` minted
    // for itself and nobody asked about.
    let unqualified = cli(
        &registry,
        &[
            "doctor",
            "--json",
            "--require-mechanism",
            "no-such-mechanism",
            "--error-format",
            "json",
        ],
    );
    let unqualified_lines = machine_stderr(
        &unqualified,
        116,
        "`doctor --json` with an unmeetable requirement",
    );
    assert_eq!(unqualified_lines[0]["kind"], json!("host_unqualified"));
    assert_eq!(unqualified_lines[0]["operation"], json!("doctor"));
    assert_eq!(
        unqualified_lines[0]["run_id"],
        Value::Null,
        "`doctor` never names a run of the caller's"
    );
    let qualification: Value =
        serde_json::from_str(String::from_utf8_lossy(&unqualified.stdout).trim())
            .expect("the qualification report is still valid JSON on stdout");
    assert_eq!(
        qualification["qualified"],
        json!(false),
        "stdout still carries the qualification report itself, unchanged: {qualification}"
    );
    lines.extend(unqualified_lines);

    check_family("error", &lines);

    let _ = fs::remove_dir_all(&dir);
}

/// `attest --json`: both verdicts a healthy platform can produce, each from a real
/// cross-process connection rather than a construction.
///
/// The `member` line is printed by a client that genuinely **is** inside the run:
/// the run's own child is `processkit-cli attest`, so the pid the runner reads off
/// the control transport is a real container member and the answer is earned rather
/// than arranged. The `not_a_member` line comes from this test process, which is
/// outside that container — the same command, the same run, a different caller, and
/// that is the only difference that produced a different verdict.
///
/// The third verdict this family's schema allows, `peer_identity_unsupported`, is
/// deliberately unpinned: it can only arise on a platform whose transport cannot name
/// a peer, and a golden fixture is generated by the real binary on the platform
/// running the tests (see `fixtures/schema/cli/README.md`). Fabricating a line for it
/// would publish an example no binary here produced.
#[test]
fn attestations_match_their_schema_and_fixture() {
    let dir = scratch("machine-attest");
    let registry = registry_dir(&dir);

    // A run whose child is the attesting client itself: an in-run caller, contained
    // by the very run it asks about.
    let inside = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "build-42", "--timeout", "60s"],
        [bin(), "attest", "--run-id", "build-42", "--json"],
    )
    .output()
    .expect("spawn a run whose child attests");
    // The child's own stdout is echoed by the runner, and the runner forwards the
    // child's exit code verbatim — so a member's `0` is what this whole invocation
    // returns.
    let mut lines = machine_output(&inside, 0, "an in-run `attest --json`");
    assert_eq!(lines.len(), 1, "one attestation line: {lines:?}");
    assert_eq!(lines[0]["verdict"], json!("member"));
    assert_eq!(lines[0]["run_id"], json!("build-42"));
    assert!(
        lines[0]["peer_pid"].is_u64(),
        "the runner names the caller from the transport: {}",
        lines[0]
    );

    // The same question from outside that container.
    let child = spawn_live_run(&dir, &registry, "build-42", &[]);
    let outside = cli(&registry, &["attest", "--run-id", "build-42", "--json"]);
    let outside_lines = machine_output(
        &outside,
        115,
        "an outside `attest --json` against a live run",
    );
    assert_eq!(outside_lines[0]["verdict"], json!("not_a_member"));
    assert_eq!(
        outside_lines[0]["attestation_version"],
        json!(1),
        "the verdict is only meaningful under a contract the client declares it reads"
    );
    lines.extend(outside_lines);

    check_family("attest", &lines);

    cancel_run(&registry, "build-42", child);
    let _ = fs::remove_dir_all(&dir);
}

/// `doctor --json`: the qualified report a healthy host produces, and the
/// unqualified one an unmeetable `--require-*` expectation produces (exit `116`,
/// `qualified: false`, and the concrete `mismatches`). Both go through the same
/// document — the shape does not change with the verdict, which is the point of
/// printing it either way.
///
/// This is the one family here whose scenario is a real, side-effecting run: the
/// command under test creates a registry, contains a process, round-trips the control
/// plane, and cleans up. It is pointed at the same isolated scratch registry every
/// other scenario uses, so it neither sees nor leaves anything in the developer's own.
///
/// The pair is also this family's differential proof, at the shape level: the two
/// lines are produced by invocations that differ **only** in a requirement flag, and
/// what differs between them is `qualified` and `mismatches` — every observed fact is
/// reported identically. (`tests/doctor.rs` makes the same point field by field on
/// the un-normalized reports; here it is visible in the committed fixture.)
#[test]
fn doctor_reports_match_their_schema_and_fixture() {
    let dir = scratch("machine-doctor");
    let registry = registry_dir(&dir);

    let qualified = cli(&registry, &["doctor", "--json"]);
    let mut lines = machine_output(&qualified, 0, "`doctor --json`");

    // A mechanism no platform reports: the requirement is unmeetable everywhere, so
    // this line's *shape* is the same on every host the tests run on, while the
    // observed facts stay whatever this host really did.
    let unqualified = cli(
        &registry,
        &[
            "doctor",
            "--json",
            "--require-mechanism",
            "no-such-mechanism",
        ],
    );
    lines.extend(machine_output(
        &unqualified,
        116,
        "`doctor --json` with an unmeetable requirement",
    ));

    assert_eq!(lines[0]["qualified"], json!(true));
    assert_eq!(lines[0]["mismatches"], json!([]));
    assert_eq!(lines[1]["qualified"], json!(false));
    assert_eq!(
        lines[1]["mismatches"].as_array().map(Vec::len),
        Some(1),
        "the unmet requirement is named: {}",
        lines[1]
    );
    assert!(
        lines[1]["failures"].as_array().is_some_and(Vec::is_empty),
        "an unmet requirement is not a failed phase: {}",
        lines[1]
    );
    assert!(
        lines[1]["diagnostics_dir"].is_null(),
        "nothing failed, so nothing is kept: {}",
        lines[1]
    );

    check_family("doctor", &lines);
    let _ = fs::remove_dir_all(&dir);
}

/// `wait --report-outcome`: the reported outcome of a run the waiter watched to
/// completion, the honest `unknown` outcome for a run it never observed live, and
/// then the aggregate `wait --all --report-outcome` array the barrier prints once
/// its snapshot clears.
///
/// All three forms belong to one test because [`check_family`] rewrites (and
/// compares) the whole `wait.jsonl` fixture in one go — the same reason
/// `inspect`'s and `control-ack`'s single and `--all` forms share a test each.
#[test]
fn wait_outcomes_match_their_schema_and_fixture() {
    let dir = scratch("machine-wait");
    let registry = registry_dir(&dir);

    let mut child = command_with_flags(
        &dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", registry.as_path())],
        &["--run-id", "build-42"],
        brief_child(),
    )
    .spawn()
    .expect("spawn the runner");
    wait_until(
        || record_is_published(&registry, "build-42"),
        Duration::from_secs(20),
        "the record for `build-42`",
    );

    let reported = cli(
        &registry,
        &["wait", "--run-id", "build-42", "--report-outcome"],
    );
    let mut lines = machine_output(&reported, 0, "`wait --report-outcome`");
    assert_eq!(
        lines[0],
        json!({
            "run_id": "build-42",
            "status": "reported",
            "code": 0,
            "source": "child_exit",
            "child_code": 0
        }),
        "the waiter reports the terminal event of the run it watched"
    );
    let _ = child.wait();

    // A run this waiter never saw live has an honest unknown outcome, not a
    // fabricated one — and still exits 0, since "no record" reads as finished.
    let unknown = cli(
        &registry,
        &["wait", "--run-id", "build-99", "--report-outcome"],
    );
    let unknown_lines = machine_output(&unknown, 0, "`wait --report-outcome` for an unknown run");
    assert_eq!(unknown_lines[0]["status"], json!("unknown"));
    lines.extend(unknown_lines);

    // The aggregate form, in its own scratch registry: `wait --all` fixes its
    // target set to the runs *confirmed live* when it starts, so giving it a fresh
    // registry (and a fresh `--jsonl` path) keeps the two scenarios above out of
    // the snapshot it reports on. Fixture order follows the schema's root `oneOf`:
    // the single-run outcomes, then the aggregate array.
    let all_dir = scratch("machine-wait-all");
    let all_registry = registry_dir(&all_dir);
    let mut all_child = command_with_flags(
        &all_dir,
        &[("PROCESSKIT_CLI_REGISTRY_DIR", all_registry.as_path())],
        &["--run-id", "build-42"],
        brief_child(),
    )
    .spawn()
    .expect("spawn the runner for the aggregate barrier");
    wait_until(
        || record_is_published(&all_registry, "build-42"),
        Duration::from_secs(20),
        "the record for the aggregate `build-42`",
    );

    // Blocking here is the point: the barrier must still see the run live when it
    // takes its snapshot, or it would honestly report the empty `[]` of a snapshot
    // with no targets — which the assertion below, and the fixture, both reject.
    let aggregate = cli(&all_registry, &["wait", "--all", "--report-outcome"]);
    let aggregate_lines = machine_output(&aggregate, 0, "`wait --all --report-outcome`");
    assert_eq!(
        aggregate_lines.len(),
        1,
        "the report is one JSON array line"
    );
    assert_eq!(
        aggregate_lines[0],
        json!([{
            "run_id": "build-42",
            "status": "reported",
            "code": 0,
            "source": "child_exit",
            "child_code": 0
        }]),
        "the barrier reports the terminal event of the one run its snapshot fixed"
    );
    lines.extend(aggregate_lines);
    let _ = all_child.wait();

    check_family("wait", &lines);
    let _ = fs::remove_dir_all(&all_dir);
    let _ = fs::remove_dir_all(&dir);
}
