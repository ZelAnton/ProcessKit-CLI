//! Arguments for the `prune` subcommand (the reaping wrapper itself lives in
//! [`crate::prune`]).

use clap::Args;

/// `prune [--json] [--dry-run] [--label <KEY=VALUE>]...`
///
/// Scans the per-user registry ([`crate::registry::Registry::prune`]) and reaps every
/// entry it can **confirm** is stale — a leftover `.json`/`.lock` pair from a runner
/// that died abruptly without running its clean-exit removal — while leaving every
/// live entry, and every entry whose liveness it could not probe, untouched. Unlike
/// `list`, prune *mutates* the registry (it deletes files), but like `list` it opens
/// the registry read-only: a missing registry has nothing to prune, so prune never
/// creates the directory or touches its permissions just to look. An empty (or
/// missing) registry is not an error — prune reports a zero tally and exits `0`.
#[derive(Debug, Args)]
pub struct PruneArgs {
    /// Emit the prune tally as a single JSON object instead of a human-readable
    /// summary line. Optional, mirroring `list` — prune has a human-readable form of
    /// its own. Combines with `--dry-run`.
    #[arg(long)]
    pub json: bool,
    /// Preview what a real prune pass would reap without deleting anything:
    /// [`crate::registry::Registry::preview_prune`] runs the exact same scan and the
    /// exact same liveness classification as a real prune, but never deletes a
    /// file — it reports the same aggregate tally a following real prune would
    /// produce, plus the confirmed-stale candidates it would reap. Combines with
    /// `--json`.
    #[arg(long)]
    pub dry_run: bool,
    /// Restrict paired registry entries to runs carrying every `KEY=VALUE` label.
    /// Repeatable; filters combine with logical AND. An explicit filter leaves
    /// record-less orphan locks alone because their ownership cannot be established.
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = crate::labels::parse)]
    pub labels: Vec<crate::labels::OperatorLabel>,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{Cli, Command};

    #[test]
    fn prune_defaults_to_no_json_and_accepts_the_flag() {
        let cli = Cli::try_parse_from(["processkit-cli", "prune"]).expect("a bare prune");
        let Command::Prune(args) = cli.command else {
            panic!("expected the prune subcommand");
        };
        assert!(
            !args.json,
            "--json is optional and defaults to off for prune"
        );

        let cli = Cli::try_parse_from(["processkit-cli", "prune", "--json"]).expect("prune --json");
        let Command::Prune(args) = cli.command else {
            panic!("expected the prune subcommand");
        };
        assert!(args.json);
    }

    #[test]
    fn prune_labels_use_the_shared_repeatable_parser() {
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "prune",
            "--label",
            "pipeline=ci",
            "--label",
            "lane=test",
        ])
        .expect("prune accepts conjunctive label filters");
        let Command::Prune(args) = cli.command else {
            panic!("expected the prune subcommand");
        };
        assert_eq!(
            args.labels,
            vec![
                crate::labels::parse("pipeline=ci").unwrap(),
                crate::labels::parse("lane=test").unwrap(),
            ]
        );
        assert!(
            Cli::try_parse_from(["processkit-cli", "prune", "--label", "not-a-label"]).is_err()
        );
    }

    /// T-199: `--dry-run` defaults to off, is accepted on its own, and combines
    /// freely with `--json` — mirroring `prune_defaults_to_no_json_and_accepts_the_flag`
    /// for the new flag.
    #[test]
    fn prune_dry_run_defaults_to_off_and_combines_with_json() {
        let cli = Cli::try_parse_from(["processkit-cli", "prune"]).expect("a bare prune");
        let Command::Prune(args) = cli.command else {
            panic!("expected the prune subcommand");
        };
        assert!(
            !args.dry_run,
            "--dry-run is optional and defaults to off for prune"
        );

        let cli =
            Cli::try_parse_from(["processkit-cli", "prune", "--dry-run"]).expect("prune --dry-run");
        let Command::Prune(args) = cli.command else {
            panic!("expected the prune subcommand");
        };
        assert!(args.dry_run);
        assert!(!args.json, "--dry-run alone must not imply --json");

        let cli = Cli::try_parse_from(["processkit-cli", "prune", "--dry-run", "--json"])
            .expect("prune --dry-run --json");
        let Command::Prune(args) = cli.command else {
            panic!("expected the prune subcommand");
        };
        assert!(args.dry_run);
        assert!(args.json, "--dry-run combines with --json");
    }
}
