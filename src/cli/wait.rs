//! Arguments for the `wait` subcommand (the registry-only barrier itself lives
//! in [`crate::wait`]).

use std::time::Duration;

use clap::Args;

use crate::labels::OperatorLabel;

use super::parse::{parse_positive_duration, parse_run_id};

/// `wait (--run-id <id> [--report-outcome] | --all [--label <KEY=VALUE>]...
/// [--report-outcome])
/// [--timeout <duration>]`
///
/// Blocks while the named run (`--run-id`) — or, in aggregate (`--all`), every run
/// confirmed live in a snapshot taken the moment the wait starts — is still live in
/// the per-user registry, and returns as soon as it is not — the supervision
/// primitive for a caller that is **not** the runner's parent and so cannot simply
/// wait on a child process (see [`crate::wait`], which implements both modes, and
/// `docs/registry.md`, "Waiting — `wait`"). Read-only in every sense: it scans the
/// registry, never connects to any run's control transport, never mutates registry
/// state, and never ends a run.
///
/// `--run-id` and `--all` are mutually exclusive (`conflicts_with`, the same clap
/// convention `RunArgs`'s stdio modes use above) and exactly one is required
/// (`required_unless_present`): a bare `wait` with neither names no target and is
/// rejected at parse time as a `USAGE` (100) form error, exactly like any other
/// malformed invocation.
#[derive(Debug, Args)]
pub struct WaitArgs {
    /// The single run to wait for. Mutually exclusive with `--all`; exactly one of
    /// the two is required.
    #[arg(
        long,
        value_name = "id",
        value_parser = parse_run_id,
        conflicts_with = "all",
        required_unless_present = "all"
    )]
    pub run_id: Option<String>,

    /// Wait for every run confirmed live in a snapshot taken the moment this
    /// invocation starts, instead of one named run — the aggregate counterpart to
    /// `--run-id`'s single-run barrier, for the typical orchestrator teardown
    /// sequence (cancel everything, wait for it all to be gone, then prune). A run
    /// that registers *after* the snapshot is out of scope for this invocation and
    /// is never waited for; re-issue `wait --all` once this one returns to catch it.
    /// Mutually exclusive with `--run-id`; exactly one of the two is required. See
    /// [`crate::wait::run_all`] and `docs/registry.md`, "Waiting — `wait`", for the
    /// full snapshot and unprobed-entry semantics.
    #[arg(long, conflicts_with = "run_id", required_unless_present = "run_id")]
    pub all: bool,

    /// With `--all`, restrict the snapshot to runs carrying this exact operator
    /// label (repeatable; multiple filters combine with logical AND).
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = crate::labels::parse, requires = "all", conflicts_with = "run_id")]
    pub labels: Vec<OperatorLabel>,

    /// After the target finishes, print its terminal `runner_exit` outcome. With
    /// `--run-id`, this is one JSON object; with `--all`, it is one JSON array in
    /// stable snapshot order with one entry per target. The wait command's own exit
    /// status remains unchanged. A target whose outcome cannot be read is reported
    /// as `status: "unknown"` with null outcome fields.
    #[arg(long)]
    pub report_outcome: bool,

    /// Give up after this long instead of waiting indefinitely. This is a deadline
    /// on **the wait**, not on any run: when it elapses, the run (or, under `--all`,
    /// every still-outstanding run) is left running, completely untouched, and
    /// `wait` exits with its own reserved [`crate::exit::WAIT_TIMEOUT`] (112) —
    /// never a run's own [`crate::exit::TIMEOUT`] (106), which would claim the
    /// runner tore the tree down. Omit it to block until the target(s) actually
    /// finish.
    ///
    /// Same grammar and parse-time validation as `run --timeout` — the very same
    /// [`parse_positive_duration`], not a second parser — so a malformed value is
    /// the same `USAGE` (100) form error, and `0` is rejected here for the same
    /// reason it is there: a zero deadline never actually waits (it expires on the
    /// first check and reports `WAIT_TIMEOUT` for any still-live run), which is a
    /// typo far more often than an intent. A caller that genuinely wants a single
    /// non-blocking check asks for the shortest real wait instead
    /// (`--timeout 1ms`).
    #[arg(long, value_name = "duration", value_parser = parse_positive_duration)]
    pub timeout: Option<Duration>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    use crate::cli::{Cli, Command};

    #[test]
    fn wait_requires_a_run_id_and_leaves_the_timeout_optional() {
        let cli = Cli::try_parse_from(["processkit-cli", "wait", "--run-id", "r1"])
            .expect("a bare wait names only the run");
        let Command::Wait(args) = cli.command else {
            panic!("expected the wait subcommand");
        };
        assert_eq!(args.run_id.as_deref(), Some("r1"));
        assert!(!args.all, "--run-id alone must not imply --all");
        assert!(!args.report_outcome, "outcome reporting is opt-in");
        assert!(
            args.timeout.is_none(),
            "omitting --timeout means wait blocks until the run finishes"
        );

        assert!(
            Cli::try_parse_from(["processkit-cli", "wait"]).is_err(),
            "one of --run-id/--all is required: there is no default target to wait for"
        );
    }

    /// T-216: `--all` is a valid alternative to `--run-id`, the two are mutually
    /// exclusive, and naming neither is rejected just like naming neither used to be
    /// impossible when `--run-id` alone was required.
    #[test]
    fn wait_all_is_an_alternative_to_run_id_and_the_two_are_mutually_exclusive() {
        let cli =
            Cli::try_parse_from(["processkit-cli", "wait", "--all"]).expect("wait --all alone");
        let Command::Wait(args) = cli.command else {
            panic!("expected the wait subcommand");
        };
        assert!(args.all);
        assert!(
            args.run_id.is_none(),
            "--all alone must not imply a --run-id"
        );

        assert!(
            Cli::try_parse_from(["processkit-cli", "wait", "--run-id", "r1", "--all"]).is_err(),
            "--run-id and --all are mutually exclusive and must be clap-rejected together"
        );
        assert!(
            Cli::try_parse_from(["processkit-cli", "wait", "--timeout", "5s"]).is_err(),
            "naming neither --run-id nor --all leaves no target and must be rejected"
        );
    }

    #[test]
    fn wait_all_combines_with_timeout_using_the_same_grammar_as_run_id() {
        let cli = Cli::try_parse_from(["processkit-cli", "wait", "--all", "--timeout", "2m"])
            .expect("wait --all --timeout 2m");
        let Command::Wait(args) = cli.command else {
            panic!("expected the wait subcommand");
        };
        assert!(args.all);
        assert_eq!(args.timeout, Some(Duration::from_secs(120)));
    }

    #[test]
    fn wait_report_outcome_is_allowed_for_single_and_aggregate_modes() {
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "wait",
            "--run-id",
            "r1",
            "--report-outcome",
        ])
        .expect("a named wait may request its terminal outcome");
        let Command::Wait(args) = cli.command else {
            panic!("expected the wait subcommand");
        };
        assert!(args.report_outcome);
        let cli = Cli::try_parse_from(["processkit-cli", "wait", "--all", "--report-outcome"])
            .expect("aggregate wait may request per-target outcomes");
        let Command::Wait(args) = cli.command else {
            panic!("expected the wait subcommand");
        };
        assert!(args.all);
        assert!(args.report_outcome);
    }

    #[test]
    fn wait_parses_a_timeout_with_the_same_grammar_as_run_timeout() {
        // `wait --timeout` reuses `parse_positive_duration` verbatim, so every form
        // `run --timeout` accepts lands here as the same ready `Duration`.
        for (raw, expected) in [
            ("30", Duration::from_secs(30)),
            ("500ms", Duration::from_millis(500)),
            ("5s", Duration::from_secs(5)),
            ("2m", Duration::from_secs(120)),
            ("1h", Duration::from_secs(3600)),
        ] {
            let cli =
                Cli::try_parse_from(["processkit-cli", "wait", "--run-id", "r1", "--timeout", raw])
                    .expect("a valid wait invocation");
            let Command::Wait(args) = cli.command else {
                panic!("expected the wait subcommand");
            };
            assert_eq!(args.timeout, Some(expected), "`wait --timeout {raw}`");
        }
    }

    #[test]
    fn wait_rejects_a_malformed_or_zero_timeout() {
        // Shared parser, shared rejections: a malformed value is a `USAGE` form
        // error at parse time, and `0` is refused for the same reason `run
        // --timeout 0` is — a deadline that never actually waits.
        for bad in ["soon", "-5", "1.5s", "5x", "0", "0ms", "0s"] {
            assert!(
                Cli::try_parse_from(["processkit-cli", "wait", "--run-id", "r1", "--timeout", bad])
                    .is_err(),
                "a malformed or degenerate `wait --timeout {bad}` must fail at parse time"
            );
        }
    }
}
