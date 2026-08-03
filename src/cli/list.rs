//! Arguments for the `list` subcommand (the renderer itself lives in
//! [`crate::list`]).

use clap::{Args, ValueEnum};

use crate::labels::OperatorLabel;

/// `list [--json] [--label <KEY=VALUE>]... [--health <health>]`
///
/// Scans the per-user registry ([`crate::registry::Registry::entries`]) and prints
/// every entry it finds, whatever its health (live/stale/unprobed) — the discovery counterpart to the
/// by-`run-id` commands above, for an operator or orchestrator that has lost (or
/// never had) a `run_id`. An empty registry is not an error: it prints an empty
/// result and exits `0`, and a single unreadable/corrupt record never blinds the
/// command to the healthy entries (the same per-record degradation
/// `Registry::entries` already applies).
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Emit one JSON object per entry (one per line) instead of a human-readable
    /// table. Unlike `inspect`/`probe`, this flag is optional — `list` has a
    /// human-readable form of its own.
    #[arg(long)]
    pub json: bool,

    /// Keep only entries carrying this exact operator label. Repeat for logical AND.
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = crate::labels::parse)]
    pub labels: Vec<OperatorLabel>,

    /// Keep only entries with this registry-health verdict.
    #[arg(long, value_name = "health", value_enum)]
    pub health: Option<ListHealth>,
}

/// Registry-health vocabulary accepted by `list --health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ListHealth {
    Live,
    Stale,
    Unprobed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    use crate::cli::{Cli, Command};

    #[test]
    fn list_defaults_to_no_json_and_accepts_the_flag() {
        let cli = Cli::try_parse_from(["processkit-cli", "list"]).expect("a bare list");
        let Command::List(args) = cli.command else {
            panic!("expected the list subcommand");
        };
        assert!(
            !args.json,
            "--json is optional and defaults to off for list"
        );
        assert!(args.labels.is_empty());
        assert!(args.health.is_none());

        let cli = Cli::try_parse_from([
            "processkit-cli",
            "list",
            "--json",
            "--label",
            "pipeline=ci",
            "--health",
            "unprobed",
        ])
        .expect("list filters");
        let Command::List(args) = cli.command else {
            panic!("expected the list subcommand");
        };
        assert!(args.json);
        assert_eq!(
            args.labels,
            vec![crate::labels::parse("pipeline=ci").unwrap()]
        );
        assert_eq!(args.health, Some(ListHealth::Unprobed));
        assert!(Cli::try_parse_from(["processkit-cli", "list", "--health", "unknown"]).is_err());
    }
}
