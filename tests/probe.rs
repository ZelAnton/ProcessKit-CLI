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
        "run:--capture-dir",
        "inspect:--json",
        "probe:--require-schema-version",
    ] {
        assert!(
            surface.contains(&token),
            "the surface must expose `{token}`: {surface:?}"
        );
    }

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
        ],
    );
    assert_eq!(surface_ok.status.code(), Some(0));
    assert_eq!(parse_report(&surface_ok)["compatible"], true);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Fail-closed outcome 1 — **path missing.** A consumer that resolves a
/// `CC_PROCESSKIT_RUN` pointing at a nonexistent file cannot even spawn the probe:
/// the OS reports `NotFound`, the distinguishable signal for "the file is gone". The
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

/// The binary under test exists — a cheap guard before the heavier scenarios.
#[test]
fn binary_under_test_exists() {
    assert!(
        Path::new(bin()).is_file(),
        "the built binary should exist at {}",
        bin()
    );
}
