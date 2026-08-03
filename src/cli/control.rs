//! Arguments for the control-plane client subcommands — `inspect`, and the
//! shared `--run-id`/`--all` target form behind `cancel` and `kill` (the clients
//! themselves live in [`crate::control`]).

use clap::Args;

use crate::labels::OperatorLabel;

use super::parse::parse_run_id;

/// `inspect (--run-id <id> | --all [--label <KEY=VALUE>]...) [--json]`
#[derive(Debug, Args)]
pub struct InspectArgs {
    /// The single run to inspect. Mutually exclusive with `--all`; exactly one
    /// target form is required.
    #[arg(long, value_name = "id", value_parser = parse_run_id, conflicts_with = "all", required_unless_present = "all")]
    pub run_id: Option<String>,

    /// Inspect every run confirmed live in one registry snapshot. Defaults to a
    /// human-readable summary plus snapshot blocks; `--json` preserves the original
    /// single-array machine report.
    #[arg(long, conflicts_with = "run_id", required_unless_present = "run_id")]
    pub all: bool,

    /// With `--all`, restrict the snapshot to runs carrying this exact operator
    /// label (repeatable; multiple filters combine with logical AND).
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = crate::labels::parse, requires = "all", conflicts_with = "run_id")]
    pub labels: Vec<OperatorLabel>,

    /// Emit the snapshot as JSON instead of a human-readable rendering. Optional,
    /// mirroring `list`/`prune` — `inspect` has a human-readable form of its own
    /// (see `src/control/render.rs::render_snapshot_human`).
    #[arg(long)]
    pub json: bool,
}

/// `cancel (--run-id <id> | --all [--label <KEY=VALUE>]...)`, `kill (--run-id <id> |
/// --all [--label <KEY=VALUE>]...)`
///
/// Shared argument for the mutating control commands (`cancel`, `kill`): act on the
/// single named run (`--run-id`), exactly as before T-217, or, in aggregate
/// (`--all`), on every run confirmed live in a snapshot taken the moment the
/// command starts — the mutating counterpart to `wait --all` (T-216), reusing its
/// exact clap convention.
///
/// `--run-id` and `--all` are mutually exclusive (`conflicts_with`) and exactly one
/// is required (`required_unless_present`): a bare `cancel`/`kill` with neither
/// names no target and is rejected at parse time as a `USAGE` (100) form error,
/// exactly like a bare `wait`.
#[derive(Debug, Args)]
pub struct TargetArgs {
    /// The single run to act on. Mutually exclusive with `--all`; exactly one of
    /// the two is required.
    #[arg(
        long,
        value_name = "id",
        value_parser = parse_run_id,
        conflicts_with = "all",
        required_unless_present = "all"
    )]
    pub run_id: Option<String>,

    /// Act on every run confirmed live in a snapshot taken the moment this
    /// invocation starts, instead of one named run — the aggregate counterpart to
    /// `--run-id`, for the typical orchestrator teardown sequence (cancel
    /// everything, wait for it all to be gone, then prune). A run that registers
    /// *after* the snapshot is out of scope for this invocation and is never acted
    /// on. Mutually exclusive with `--run-id`; exactly one of the two is required.
    /// See [`crate::control::cancel_all`] / [`crate::control::kill_all`] and
    /// `docs/control-plane.md` for the snapshot and per-run report semantics.
    #[arg(long, conflicts_with = "run_id", required_unless_present = "run_id")]
    pub all: bool,

    /// With `--all`, restrict the snapshot to runs carrying this exact operator
    /// label (repeatable; multiple filters combine with logical AND).
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = crate::labels::parse, requires = "all", conflicts_with = "run_id")]
    pub labels: Vec<OperatorLabel>,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{Cli, Command};

    #[test]
    fn inspect_requires_run_id_but_json_is_optional() {
        assert!(
            Cli::try_parse_from(["processkit-cli", "inspect", "--run-id", "r1", "--json"]).is_ok()
        );

        let cli = Cli::try_parse_from(["processkit-cli", "inspect", "--run-id", "r1"])
            .expect("--json is optional and defaults to off for inspect");
        let Command::Inspect(args) = cli.command else {
            panic!("expected the inspect subcommand");
        };
        assert_eq!(args.run_id.as_deref(), Some("r1"));
        assert!(!args.all);
        assert!(args.labels.is_empty());
        assert!(
            !args.json,
            "--json is optional and defaults to off for inspect"
        );

        assert!(
            Cli::try_parse_from(["processkit-cli", "inspect", "--json"]).is_err(),
            "--run-id or --all is required"
        );
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "inspect",
            "--all",
            "--json",
            "--label",
            "pipeline=ci",
        ])
        .expect("aggregate inspect parses");
        let Command::Inspect(args) = cli.command else {
            panic!("expected inspect");
        };
        assert!(args.all && args.json && args.run_id.is_none());
        assert_eq!(args.labels.len(), 1);
        let cli = Cli::try_parse_from(["processkit-cli", "inspect", "--all"])
            .expect("aggregate inspect defaults to human output");
        let Command::Inspect(args) = cli.command else {
            panic!("expected inspect");
        };
        assert!(args.all && !args.json && args.run_id.is_none());
    }

    #[test]
    fn cancel_and_kill_require_a_run_id_or_all() {
        assert!(Cli::try_parse_from(["processkit-cli", "cancel", "--run-id", "r1"]).is_ok());
        assert!(Cli::try_parse_from(["processkit-cli", "kill", "--run-id", "r1"]).is_ok());
        assert!(
            Cli::try_parse_from(["processkit-cli", "cancel"]).is_err(),
            "one of --run-id/--all is required: there is no default target"
        );
        assert!(
            Cli::try_parse_from(["processkit-cli", "kill"]).is_err(),
            "one of --run-id/--all is required: there is no default target"
        );
    }

    /// T-217: `--all` is a valid alternative to `--run-id` for both `cancel` and
    /// `kill`, the two are mutually exclusive, and naming neither is rejected — the
    /// same clap convention `WaitArgs`/T-216 already established for `wait --all`
    /// (see `wait_all_is_an_alternative_to_run_id_and_the_two_are_mutually_exclusive`
    /// above).
    #[test]
    fn cancel_and_kill_all_is_an_alternative_to_run_id_and_the_two_are_mutually_exclusive() {
        for sub in ["cancel", "kill"] {
            let cli = Cli::try_parse_from(["processkit-cli", sub, "--all"])
                .unwrap_or_else(|err| panic!("{sub} --all alone must parse: {err}"));
            let (all, run_id) = match (sub, cli.command) {
                ("cancel", Command::Cancel(args)) => (args.all, args.run_id),
                ("kill", Command::Kill(args)) => (args.all, args.run_id),
                _ => panic!("expected the {sub} subcommand"),
            };
            assert!(all);
            assert!(
                run_id.is_none(),
                "--all alone must not imply a --run-id for {sub}"
            );

            assert!(
                Cli::try_parse_from(["processkit-cli", sub, "--run-id", "r1", "--all"]).is_err(),
                "--run-id and --all are mutually exclusive and must be clap-rejected \
                 together for {sub}"
            );
        }
    }
}
