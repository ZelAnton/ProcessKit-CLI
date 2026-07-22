//! Integration-test home for real-binary containment scenarios.

mod common;

use std::process::Command;

/// Mirrors the private `exit::USAGE` constant (100) — the runner's own
/// invalid-command-line exit code (see `docs/exit-codes.md`).
const USAGE: i32 = 100;

#[test]
fn roadmap_is_present() {
    assert!(std::path::Path::new("docs/ROADMAP.md").is_file());
}

/// A bare invocation with no subcommand at all is an invalid command line, not a
/// successful no-op: clap reports it via
/// `ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand`, which `report_parse_error`
/// maps onto the runner's own `exit::USAGE` (100) rather than clap's default
/// "successful help" exit of 0 — a caller that forgets to pass a subcommand must
/// see a failure, not a silent success.
#[test]
fn no_subcommand_exits_usage() {
    let out = Command::new(common::bin())
        .output()
        .expect("spawn the runner binary with no arguments");
    assert_eq!(
        out.status.code(),
        Some(USAGE),
        "a bare invocation with no subcommand must exit USAGE (100), not succeed"
    );
}
