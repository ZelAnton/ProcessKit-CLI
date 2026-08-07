//! Through-the-binary tests for the `probe` preflight subcommand — the in-binary
//! half of the fail-closed launcher contract. They drive the
//! *built binary* (as every test here does, `AGENTS.md`, "Testing tiers"), because
//! the value is the binary's contract, not the library.
//!
//! Coverage mirrors the contract's guarantees:
//!
//! - a bare `probe --json` on the freshly built current binary is a deterministic,
//!   machine-readable self-report (version, `schema_version`, exit-code band, CLI
//!   surface) and exits `0`;
//! - the three fail-closed outcomes are each **distinct and parseable**: a missing
//!   path and a present-but-not-executable path fail the *spawn* with
//!   distinguishable OS errors, and a present-executable-but-incompatible binary
//!   (a simulated surface mismatch) prints `compatible:false` and exits with the
//!   reserved `PROBE_INCOMPATIBLE` code (110) — never a silent "ok", never a
//!   generic error;
//! - the probe has no side effects: it spawns no child and writes nothing to its
//!   working directory.

mod common;

use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

use common::{bin, scratch};
use serde_json::Value;

/// The reserved runner exit-code for an incompatible preflight (`docs/exit-codes.md`).
const PROBE_INCOMPATIBLE: i32 = 110;

/// Invoke `probe` on the built binary with `extra` args and wait for it to finish.
/// `cwd` is where the probe runs — a fresh scratch dir, so a test can assert the
/// probe left nothing behind.
fn probe(cwd: &Path, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(bin());
    cmd.current_dir(cwd);
    cmd.arg("probe").arg("--json");
    cmd.args(extra);
    cmd.output().expect("spawn the probe")
}

/// Parse the single JSON line the probe prints to stdout.
fn parse_report(output: &std::process::Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_else(|| panic!("the probe prints a JSON report line; got stdout {stdout:?}"));
    serde_json::from_str(line)
        .unwrap_or_else(|err| panic!("the probe report is valid JSON: {err}; line {line:?}"))
}

/// A bare `probe --json` is a healthy, deterministic self-report: it names the
/// binary, carries this build's exact version, the current `schema_version`, the
/// reserved `100..=119` band, a CLI surface listing every subcommand and flag, is
/// `compatible` with no mismatches, and exits `0`. This is the golden success the
/// contract promises a consumer running the probe on a good candidate.
#[test]
fn probe_reports_a_consistent_compatible_surface() {
    let dir = scratch("probe-ok");
    let out = probe(&dir, &[]);
    assert_eq!(out.status.code(), Some(0), "a healthy probe exits 0");
    assert!(
        out.stderr.is_empty(),
        "a compatible probe writes nothing to stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report = parse_report(&out);
    assert_eq!(report["probe_version"], 1);
    assert_eq!(
        report["binary"], "processkit-cli",
        "the report names the binary so a consumer can confirm the candidate: {report}"
    );
    assert_eq!(
        report["version"],
        env!("CARGO_PKG_VERSION"),
        "the report carries this build's exact version: {report}"
    );
    assert!(
        report["schema_version"].as_u64().is_some(),
        "the report carries a numeric schema_version: {report}"
    );
    assert_eq!(report["exit_code_band"]["start"], 100);
    assert_eq!(report["exit_code_band"]["end"], 119);
    assert_eq!(report["compatible"], true);
    assert_eq!(
        report["mismatches"].as_array().map(Vec::len),
        Some(0),
        "a compatible report has no mismatches: {report}"
    );

    // The surface tracks the real CLI: every subcommand and representative flags.
    let surface: Vec<&str> = report["surface"]
        .as_array()
        .expect("surface is an array")
        .iter()
        .map(|v| v.as_str().expect("each surface token is a string"))
        .collect();
    for token in [
        "run",
        "inspect",
        "cancel",
        "kill",
        "probe",
        "run:--jsonl",
        "run:--label",
        "run:--env-file",
        "run:--capture-dir",
        "run:--inherit-stdio",
        "run:--windows-graceful-ctrl-break",
        "run:--inherit-stdin",
        "run:--stdin-file",
        "inspect:--json",
        "cancel:--label",
        "kill:--label",
        "probe:--require-schema-version",
        // The `wait` subcommand and its flags (T-197) appear here automatically too:
        // the surface is derived from the live clap tree, so a new subcommand needs
        // no hand-maintained token list anywhere in production code.
        "wait",
        "wait:--run-id",
        "wait:--label",
        "wait:--timeout",
        // And for `events` (T-296), through the real binary this time: the
        // subcommand and every one of its flags reach a consumer's
        // `--require-surface` preflight with no hand-maintained token list behind
        // them.
        "events",
        "events:--run-id",
        "events:--file",
        "events:--json",
        "events:--follow",
        "events:--validate",
        // Same story for the `run --detach` flag (T-198): a new flag on an existing
        // subcommand enters the advertised surface with no production-code edit.
        "run:--detach",
        // And for `probe --print-schema` (T-213) itself.
        "probe:--print-schema",
        // And for `run --snapshot-interval` (T-298), the periodic
        // `members_snapshot` cadence: still no hand-maintained token list in
        // production code, so the flag advertises itself.
        "run:--snapshot-interval",
        // `--error-format` (T-305) is the first **global** option this CLI has, and
        // it is the one case that did need a production-code change in
        // `src/probe.rs`: a global is declared once on `Cli`, not on any subcommand,
        // so the derivation had to learn a new *category* of surface rather than
        // pick up a new instance of an existing one. What did not change is the
        // token grammar or any hand-maintained list — the globals are still read off
        // the live clap tree, and every subcommand really does accept the flag
        // (`src/cli/mod.rs`'s `every_subcommand_accepts_the_global_error_format`),
        // so none of these tokens is a promise the binary does not keep.
        "run:--error-format",
        "inspect:--error-format",
        "cancel:--error-format",
        "kill:--error-format",
        "wait:--error-format",
        "events:--error-format",
        "list:--error-format",
        "prune:--error-format",
        "probe:--error-format",
        // `attest` (T-306) is the third whole subcommand to enter the surface with
        // no hand-maintained token list, flags and global included.
        "attest",
        "attest:--run-id",
        "attest:--json",
        "attest:--error-format",
        // `doctor` (T-307) is the fourth, and the first whose *purpose* is the same
        // as this command's: it qualifies the host where `probe` qualifies the
        // binary. It still enters the surface the same way every other subcommand
        // does — `src/probe.rs` was not touched to add it — so a consumer's
        // fail-closed preflight can require the runtime-qualification command
        // exactly as it requires any flag.
        "doctor",
        "doctor:--json",
        "doctor:--timeout",
        "doctor:--check-resource-controller",
        "doctor:--require-mechanism",
        "doctor:--require-abrupt-cleanup",
        "doctor:--require-resource-controller",
        "doctor:--scratch-child",
        "doctor:--error-format",
    ] {
        assert!(
            surface.contains(&token),
            "the surface must expose `{token}`: {surface:?}"
        );
    }

    // The one **capability** token (T-306), and the one entry here that is not a
    // spelling the parser knows: `attest:peer-identity` says this build can obtain a
    // kernel-authenticated peer identity on this platform, which is what makes
    // `attest`'s verdict possible at all. It carries no `--` precisely so it is never
    // mistaken for a flag. Every target this project releases for (Windows, Linux,
    // macOS) has that facility, so through the real binary it must be advertised
    // here; that a consumer can then *require* it, and that requiring it really is
    // fail-closed, is asserted in
    // `incompatible_band_and_surface_fail_closed_and_real_ones_pass` below.
    assert!(
        surface.contains(&"attest:peer-identity"),
        "the platforms this project ships for all name their control-plane peers, so \
         the capability must be advertised: {surface:?}"
    );

    // The second capability token (T-317), and the one that shows the `--`-less form is
    // a *category* rather than a synonym for "platform-dependent":
    // `run:resource-summary` says this build's `run` emits the terminal
    // `resource_summary` event. Unlike its sibling above it has no platform condition
    // at all — every build that has the event has the token — which is exactly why an
    // adapter needs it: an event's presence is otherwise undiscoverable until a run has
    // already finished without it.
    assert!(
        surface.contains(&"run:resource-summary"),
        "every build that emits resource_summary advertises it: {surface:?}"
    );
    // The grammar rule, held from the consumer's side over the whole `run:` namespace
    // (the production-side twin lives in `src/probe.rs`'s own tests): a `run:` token
    // either names a long flag or is precisely this one capability. Nothing may appear
    // in between, which is what stops a capability from being spelled like a flag.
    let stray: Vec<&&str> = surface
        .iter()
        .filter(|token| {
            token.starts_with("run:")
                && !token.starts_with("run:--")
                && **token != "run:resource-summary"
        })
        .collect();
    assert!(
        stray.is_empty(),
        "a `run:` token is a long flag or the one capability, never a third shape: \
         {stray:?}"
    );

    // No side effects: the probe spawned nothing and wrote nothing to its cwd.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .expect("read the probe cwd")
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "the probe must not create files in its working directory: {leftovers:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The probe is idempotent and deterministic: two runs of the current binary print
/// byte-for-byte the same report. A consumer can therefore cache or compare it.
#[test]
fn probe_is_deterministic_across_runs() {
    let dir = scratch("probe-determinism");
    let first = probe(&dir, &[]);
    let second = probe(&dir, &[]);
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(
        first.stdout, second.stdout,
        "the probe report is deterministic across runs of the same binary"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fail-closed outcome 3 — **present, executable, but incompatible.** A simulated
/// version mismatch (requiring a `schema_version` one past what this binary emits)
/// makes the probe report `compatible:false` with a concrete, parseable mismatch and
/// exit with the reserved `PROBE_INCOMPATIBLE` (110) — never a silent "ok".
#[test]
fn incompatible_schema_version_fails_closed_with_110() {
    let dir = scratch("probe-schema-mismatch");

    // Learn the real schema_version from a healthy probe, then require one past it —
    // a version this binary cannot satisfy, exactly the "old/incompatible file" the
    // consumer must reject.
    let healthy = parse_report(&probe(&dir, &[]));
    let schema = healthy["schema_version"]
        .as_u64()
        .expect("numeric schema_version");
    let unsupported = (schema + 1).to_string();

    let out = probe(&dir, &["--require-schema-version", &unsupported]);
    assert_eq!(
        out.status.code(),
        Some(PROBE_INCOMPATIBLE),
        "an unmet schema requirement exits with the reserved PROBE_INCOMPATIBLE code"
    );
    let report = parse_report(&out);
    assert_eq!(
        report["compatible"], false,
        "the report explicitly says it is not compatible: {report}"
    );
    let mismatches = report["mismatches"].as_array().expect("mismatches array");
    assert_eq!(mismatches.len(), 1, "one concrete reason: {report}");
    assert!(
        mismatches[0]
            .as_str()
            .is_some_and(|m| m.contains(&unsupported) && m.contains(&schema.to_string())),
        "the mismatch names the requested and the actual schema_version: {report}"
    );
    // A distinguishable result, not a generic error: stderr explains the code.
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("incompatible"),
        "the incompatibility is stated on stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The other two surface dimensions fail closed the same way: a differing exit-code
/// band and an absent CLI surface token each yield `compatible:false` and exit 110,
/// while the exact reserved band and a real token are compatible (exit 0).
#[test]
fn incompatible_band_and_surface_fail_closed_and_real_ones_pass() {
    let dir = scratch("probe-band-surface");

    // A narrowed band is incompatible.
    let band = probe(&dir, &["--require-exit-code-band", "100-118"]);
    assert_eq!(band.status.code(), Some(PROBE_INCOMPATIBLE));
    assert_eq!(parse_report(&band)["compatible"], false);

    // The exact reserved band is compatible.
    let band_ok = probe(&dir, &["--require-exit-code-band", "100-119"]);
    assert_eq!(band_ok.status.code(), Some(0));
    assert_eq!(parse_report(&band_ok)["compatible"], true);

    // A bogus surface token is incompatible.
    let surface = probe(&dir, &["--require-surface", "run:--not-a-real-flag"]);
    assert_eq!(surface.status.code(), Some(PROBE_INCOMPATIBLE));
    assert_eq!(parse_report(&surface)["compatible"], false);

    // Real subcommand and flag tokens are compatible, several at once.
    let surface_ok = probe(
        &dir,
        &[
            "--require-surface",
            "probe",
            "--require-surface",
            "run:--capture-dir",
            "--require-surface",
            "run:--inherit-stdio",
            "--require-surface",
            "run:--inherit-stdin",
            "--require-surface",
            "run:--stdin-file",
        ],
    );
    assert_eq!(surface_ok.status.code(), Some(0));
    assert_eq!(parse_report(&surface_ok)["compatible"], true);

    // The capability token behaves exactly like every other requirement — which is
    // the whole reason it is published as one (T-306). An adapter that will gate work
    // on `attest` asks for it here, at preflight, and gets a real verdict rather than
    // discovering mid-run that the runner cannot name its callers. On this platform
    // the capability is present, so the check passes; on one where it is absent the
    // very same invocation is `compatible: false` and exit 110, like the bogus token
    // above — a missing capability can never read as a satisfied requirement.
    let capability = probe(
        &dir,
        &[
            "--require-surface",
            "attest",
            "--require-surface",
            "attest:--run-id",
            "--require-surface",
            "attest:peer-identity",
        ],
    );
    assert_eq!(capability.status.code(), Some(0));
    assert_eq!(parse_report(&capability)["compatible"], true);

    // A capability spelled like a flag is not a capability: the grammar is part of
    // the contract, so `attest:--peer-identity` is simply an unknown token and fails
    // closed.
    let mistyped = probe(&dir, &["--require-surface", "attest:--peer-identity"]);
    assert_eq!(mistyped.status.code(), Some(PROBE_INCOMPATIBLE));
    assert_eq!(parse_report(&mistyped)["compatible"], false);

    // The second capability token, round-tripped the way an adapter that will read the
    // consumption summary actually preflights it — alongside the flags it will use, in
    // one invocation, exactly like the flag tokens beside it.
    let summary = probe(
        &dir,
        &[
            "--require-surface",
            "run",
            "--require-surface",
            "run:--jsonl",
            "--require-surface",
            "run:resource-summary",
        ],
    );
    assert_eq!(
        summary.status.code(),
        Some(0),
        "requiring the resource_summary capability passes on a build that has it; \
         stderr: {}",
        String::from_utf8_lossy(&summary.stderr)
    );
    assert_eq!(parse_report(&summary)["compatible"], true);

    // And the same grammar trap on this token: spelled with `--` it is not the
    // capability, it is an unknown flag, and it fails closed. Requiring it must not
    // accidentally be satisfied by the real capability's presence.
    let mistyped_summary = probe(&dir, &["--require-surface", "run:--resource-summary"]);
    assert_eq!(
        mistyped_summary.status.code(),
        Some(PROBE_INCOMPATIBLE),
        "a capability spelled as a flag is an unknown token, not a satisfied \
         requirement; stdout: {}",
        String::from_utf8_lossy(&mistyped_summary.stdout)
    );
    assert_eq!(parse_report(&mistyped_summary)["compatible"], false);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Fail-closed outcome 1 — **path missing.** A consumer that resolves a
/// nonexistent candidate cannot even spawn the probe: the OS reports `NotFound`,
/// the distinguishable signal for "the file is gone". The
/// consumer must fail closed here, never fall back to an uncontained launch.
#[test]
fn a_missing_path_fails_the_spawn_with_not_found() {
    let dir = scratch("probe-missing");
    let missing = dir.join("no_such_processkit_cli_binary");
    assert!(!missing.exists(), "the fixture path must not exist");

    let err = Command::new(&missing)
        .arg("probe")
        .arg("--json")
        .output()
        .expect_err("spawning a missing path must fail");
    assert_eq!(
        err.kind(),
        ErrorKind::NotFound,
        "a missing launch target is distinguishably NotFound: {err:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Fail-closed outcome 2 — **present but not executable.** A file that exists but
/// cannot be executed also fails the *spawn*, but with an error distinct from
/// `NotFound` — so the consumer tells "the file is there but unusable" apart from
/// "the file is gone", and fails closed on both. On Unix the error is precisely
/// `PermissionDenied`; on Windows a non-executable file is rejected by the loader
/// with a non-`NotFound` error.
#[test]
fn a_non_executable_path_fails_the_spawn_distinctly_from_missing() {
    let dir = scratch("probe-nonexec");
    let file = dir.join("not_a_binary");
    std::fs::write(&file, b"this is not an executable\n").expect("write the fixture file");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Readable but not executable by anyone.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644))
            .expect("drop the execute bit");
    }

    assert!(file.exists(), "the fixture file must exist");
    let err = Command::new(&file)
        .arg("probe")
        .arg("--json")
        .output()
        .expect_err("spawning a non-executable file must fail");
    assert_ne!(
        err.kind(),
        ErrorKind::NotFound,
        "a present-but-unusable file is distinguishable from a missing one: {err:?}"
    );
    #[cfg(unix)]
    assert_eq!(
        err.kind(),
        ErrorKind::PermissionDenied,
        "a non-executable file on Unix is PermissionDenied: {err:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `probe --print-schema` prints the exact bytes of the schema document embedded
/// in this binary at build time — byte-for-byte identical to
/// `fixtures/schema/v1/schema.json` on disk, read independently here at test
/// time (not via the crate's own `include_str!`) so a real drift between the
/// two would actually fail this test. Also confirms the flag's side-effect-free,
/// exit-0 contract: no stderr, no leftover files.
#[test]
fn print_schema_prints_the_fixture_verbatim() {
    let dir = scratch("probe-print-schema");
    let out = probe(&dir, &["--print-schema"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "printing the schema succeeds: {out:?}"
    );
    assert!(
        out.stderr.is_empty(),
        "no side-effect diagnostics on stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/schema/v1/schema.json");
    let fixture = std::fs::read(&fixture_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", fixture_path.display()));
    assert_eq!(
        out.stdout, fixture,
        "probe --print-schema must print the schema byte-for-byte, matching the fixture on disk"
    );

    // No side effects, same as a bare probe.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .expect("read the probe cwd")
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "probe --print-schema must not create files in its working directory: {leftovers:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--print-schema` surface-exposes itself in the ordinary probe report too (the
/// live clap-derived token, see `src/probe.rs`'s `surface_tokens`), and a
/// consumer can require it via `--require-surface` like any other token.
#[test]
fn print_schema_appears_in_the_advertised_surface() {
    let dir = scratch("probe-print-schema-surface");
    let out = probe(&dir, &["--require-surface", "probe:--print-schema"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the surface must already advertise probe:--print-schema: {out:?}"
    );
    assert_eq!(parse_report(&out)["compatible"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The runtime-qualification command and its flags reach the advertised surface the
/// same way every other subcommand does — through the live clap tree, with no edit to
/// `src/probe.rs` — so a consumer can require them in the very preflight that decides
/// whether to run `doctor` at all.
///
/// Driven as a real `--require-surface` run rather than by reading the array, because
/// what a consumer actually does is ask the binary to *verify* the token and act on
/// the exit code: this asserts the fail-closed gate, not just the report's contents.
/// It also pins the one thing `doctor` must **not** do to `probe`: a bare probe with
/// these requirements still spawns nothing and leaves nothing behind.
#[test]
fn doctor_appears_in_the_advertised_surface() {
    let dir = scratch("probe-doctor-surface");
    let out = probe(
        &dir,
        &[
            "--require-surface",
            "doctor",
            "--require-surface",
            "doctor:--json",
            "--require-surface",
            "doctor:--require-abrupt-cleanup",
            "--require-surface",
            "doctor:--check-resource-controller",
            "--require-surface",
            "doctor:--scratch-child",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "the surface must already advertise `doctor` and its flags: {out:?}"
    );
    assert_eq!(parse_report(&out)["compatible"], true);

    // Requiring the *side-effecting* command from the side-effect-free one changes
    // nothing about the latter: still no child, still nothing written.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .expect("read the probe cwd")
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "requiring `doctor` must not make `probe` do anything: {leftovers:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The binary under test exists — a cheap guard before the heavier scenarios.
#[test]
fn binary_under_test_exists() {
    assert!(
        Path::new(bin()).is_file(),
        "the built binary should exist at {}",
        bin()
    );
}
