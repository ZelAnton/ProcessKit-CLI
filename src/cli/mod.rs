//! Command-line surface for processkit-cli.
//!
//! This is the *CLI flags* half of the compatibility surface fixed by
//! `AGENTS.md`; the shapes here are normative and mirror README's "Command
//! interface".
//!
//! # Layout
//!
//! This module is a directory rather than a single file: the top-level
//! [`Cli`]/[`Command`] shapes live here, each subcommand family's argument
//! struct lives in the file named after the module that implements that
//! subcommand — `run.rs` ([`RunArgs`]), `control.rs` ([`InspectArgs`] and the
//! shared [`TargetArgs`] behind `cancel`/`kill`), `wait.rs` ([`WaitArgs`]),
//! `list.rs` ([`ListArgs`]), `prune.rs` ([`PruneArgs`]), and `probe.rs`
//! ([`ProbeArgs`]) — and the hand-written value parsers those arguments share
//! live in `parse.rs`. Each file carries the unit tests for the shapes it
//! defines.
//!
//! The submodules are private and everything they define is re-exported here, so
//! `crate::cli::<Item>` stays the single path to every CLI type and parser —
//! unchanged for `src/main.rs`, the subcommand modules, the fuzz targets
//! (`fuzz/fuzz_targets/cli_parsers.rs`), and `build.rs`, which loads this very
//! file to generate completions and man pages from the live parser.

mod control;
mod list;
mod parse;
mod probe;
mod prune;
mod run;
mod wait;

use clap::{Parser, Subcommand};

pub use control::{InspectArgs, TargetArgs};
pub use list::{ListArgs, ListHealth};
pub use probe::ProbeArgs;
pub use prune::PruneArgs;
pub use run::{CaptureOverflowPolicy, RunArgs};
pub use wait::WaitArgs;

/// The shared value parsers, re-exported on this module's own path so they keep
/// the exact `processkit_cli::cli::parse_*` spelling
/// `fuzz/fuzz_targets/cli_parsers.rs` drives them through: the split moved where
/// they are defined, never how they are reached or what they accept.
#[doc(hidden)]
pub use parse::{
    parse_cpu_quota, parse_duration, parse_env_kv, parse_exit_code_band, parse_max_processes,
    parse_positive_duration, parse_run_id, parse_size,
};

/// Top-level parser: one required subcommand, no global options.
#[derive(Debug, Parser)]
#[command(
    name = "processkit-cli",
    version,
    about = "Run one shell-free command inside ProcessKit's containment boundary.",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// The commands that make up the runner's control surface.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a program inside a ProcessKit container and report its lifecycle.
    Run(Box<RunArgs>),
    /// Query a live run over local IPC.
    Inspect(InspectArgs),
    /// Ask a live run to cancel (graceful where supported, then a hard kill), by
    /// `--run-id` or, in aggregate, `--all`.
    Cancel(TargetArgs),
    /// Hard-kill a live run's container immediately, by `--run-id` or, in
    /// aggregate, `--all`.
    Kill(TargetArgs),
    /// Block until a run recorded in the per-user registry has finished.
    Wait(WaitArgs),
    /// List every run recorded in the per-user registry, whatever its health
    /// (live/stale/unprobed).
    List(ListArgs),
    /// Reap the registry's confirmed-stale entries — the leftover records of runners
    /// that died abruptly — while never touching a live run's entry.
    Prune(PruneArgs),
    /// Report this binary's compatibility surface for a consumer's fail-closed
    /// compatibility preflight — no run, no child, no side effects.
    Probe(ProbeArgs),
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // Catches misconfigured derive attributes (conflicting names, bad
        // num_args, etc.) that would otherwise only surface at runtime.
        Cli::command().debug_assert();
    }

    #[test]
    fn labels_parse_for_runs_and_only_scope_aggregate_commands() {
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--label",
            "batch=42",
            "--label",
            "lane=test",
            "--",
            "true",
        ])
        .expect("valid run labels");
        let Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(args.labels.len(), 2);

        for sub in ["cancel", "kill", "wait"] {
            assert!(
                Cli::try_parse_from(["processkit-cli", sub, "--all", "--label", "batch=42"])
                    .is_ok(),
                "{sub} --all accepts a label filter"
            );
            assert!(
                Cli::try_parse_from([
                    "processkit-cli",
                    sub,
                    "--run-id",
                    "r1",
                    "--label",
                    "batch=42"
                ])
                .is_err(),
                "{sub} labels require --all"
            );
        }
    }

    #[test]
    fn every_by_id_command_rejects_unsafe_run_ids_at_parse_time() {
        let too_long = "x".repeat(257);
        for bad in ["", "line\nbreak", "bidi\u{202e}override", &too_long] {
            let commands = [
                vec![
                    "processkit-cli",
                    "run",
                    "--run-id",
                    bad,
                    "--jsonl",
                    "events.jsonl",
                    "--",
                    "true",
                ],
                vec!["processkit-cli", "inspect", "--run-id", bad],
                vec!["processkit-cli", "cancel", "--run-id", bad],
                vec!["processkit-cli", "kill", "--run-id", bad],
                vec!["processkit-cli", "wait", "--run-id", bad],
            ];
            for command in commands {
                assert!(
                    Cli::try_parse_from(command).is_err(),
                    "every by-id command must reject {bad:?}"
                );
            }
        }
    }
}
