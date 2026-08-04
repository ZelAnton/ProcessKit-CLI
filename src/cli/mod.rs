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
//! shared [`TargetArgs`] behind `cancel`/`kill`), `attest.rs` ([`AttestArgs`]),
//! `wait.rs` ([`WaitArgs`]),
//! `list.rs` ([`ListArgs`]), `prune.rs` ([`PruneArgs`]), `probe.rs`
//! ([`ProbeArgs`]), and `events.rs` ([`EventsArgs`]) — and the hand-written value
//! parsers those arguments share live in `parse.rs`. Each file carries the unit
//! tests for the shapes it defines. Two files' names do not match their
//! implementing module: `events.rs`, because the `events` subcommand is implemented
//! by [`crate::events_cmd`] (the plain `events` module name already belongs to the
//! JSONL emitter that command reads back), and `attest.rs`, whose client lives in
//! [`crate::control`] alongside the other control-plane verbs — it has a file of its
//! own because its argument shape is the opposite of `control.rs`'s shared
//! `--run-id`/`--all` target form, and deliberately so (see [`AttestArgs`]).
//!
//! The submodules are private and everything they define is re-exported here, so
//! `crate::cli::<Item>` stays the single path to every CLI type and parser —
//! unchanged for `src/main.rs`, the subcommand modules, the fuzz targets
//! (`fuzz/fuzz_targets/cli_parsers.rs`), and `build.rs`, which loads this very
//! file to generate completions and man pages from the live parser.

mod attest;
mod control;
mod events;
mod list;
mod parse;
mod probe;
mod prune;
mod run;
mod wait;

use clap::{Parser, Subcommand, ValueEnum};

pub use attest::AttestArgs;
pub use control::{InspectArgs, TargetArgs};
pub use events::EventsArgs;
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

/// Top-level parser: one required subcommand, plus the one global option every
/// subcommand shares ([`Cli::error_format`]).
#[derive(Debug, Parser)]
#[command(
    name = "processkit-cli",
    version,
    about = "Run one shell-free command inside ProcessKit's containment boundary.",
    long_about = None
)]
pub struct Cli {
    /// How a runner-own failure is reported on stderr. `human` (the default) prints
    /// the historical `processkit-cli: <message>` prose; `json` prints exactly one
    /// bounded, versioned JSON object instead, so an adapter can branch on stable
    /// fields (`code`, `kind`, `retryable`) rather than parsing prose.
    ///
    /// Global on purpose — it is accepted before or after the subcommand, and every
    /// subcommand honors it. It never changes stdout, an exit code, or the default
    /// prose; clap's own parse-time usage errors stay human-readable in v1. See
    /// [`crate::error_envelope`] and `docs/exit-codes.md`.
    #[arg(
        long,
        global = true,
        value_name = "format",
        value_enum,
        default_value_t = ErrorFormat::Human
    )]
    pub error_format: ErrorFormat,

    #[command(subcommand)]
    pub command: Command,
}

/// How a runner-own failure is rendered on stderr — the value of `--error-format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum ErrorFormat {
    /// The historical free-text line, unchanged byte for byte.
    #[default]
    Human,
    /// One bounded, versioned JSON object per failed invocation (see
    /// [`crate::error_envelope`] and `fixtures/schema/cli/error.schema.json`).
    Json,
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
    /// Ask a live run whether this very process is inside its container — a fact the
    /// runner checks against the kernel's own view of who connected, never a claim
    /// the caller makes about itself.
    Attest(AttestArgs),
    /// Block until a run recorded in the per-user registry has finished.
    Wait(WaitArgs),
    /// Read back a run's JSONL lifecycle stream: render it, follow it, pass it
    /// through, or check it against the embedded event schema.
    Events(EventsArgs),
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

impl Command {
    /// The subcommand's own name, exactly as it is spelled on the command line.
    ///
    /// This is what a failure envelope reports as its `operation`
    /// ([`crate::error_envelope`]), so it is derived from this enum rather than
    /// re-typed anywhere else, and the `match` is exhaustive: a new subcommand
    /// cannot be added without naming it here — and, through the envelope's schema
    /// test, without publishing it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Run(_) => "run",
            Self::Inspect(_) => "inspect",
            Self::Cancel(_) => "cancel",
            Self::Kill(_) => "kill",
            Self::Attest(_) => "attest",
            Self::Wait(_) => "wait",
            Self::Events(_) => "events",
            Self::List(_) => "list",
            Self::Prune(_) => "prune",
            Self::Probe(_) => "probe",
        }
    }

    /// The run id this invocation named, if it named one.
    ///
    /// `None` covers every honest way there is no single run to name: an `--all`
    /// fan-out (whose targets are a snapshot, not one id), a whole-registry command
    /// (`list`/`prune`), the self-contained `probe`, and a `run` that let the runner
    /// generate an id — the generated value is minted inside the run itself and is
    /// not knowable here, so reporting `null` is the truthful answer rather than a
    /// guess.
    pub fn target_run_id(&self) -> Option<&str> {
        match self {
            Self::Run(args) => args.run_id.as_deref(),
            Self::Inspect(args) => args.run_id.as_deref(),
            Self::Cancel(args) | Self::Kill(args) => args.run_id.as_deref(),
            // The one by-id command whose target is not optional: `attest` has no
            // aggregate form to be absent for (see `AttestArgs`).
            Self::Attest(args) => Some(&args.run_id),
            Self::Wait(args) => args.run_id.as_deref(),
            Self::Events(args) => args.run_id.as_deref(),
            Self::List(_) | Self::Prune(_) | Self::Probe(_) => None,
        }
    }
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
    fn error_format_defaults_to_the_historical_prose() {
        // The envelope is strictly opt-in: an invocation that says nothing about it
        // gets exactly what every release before it printed.
        let cli = Cli::try_parse_from(["processkit-cli", "list"]).expect("a bare list parses");
        assert_eq!(cli.error_format, ErrorFormat::Human);
    }

    #[test]
    fn error_format_is_accepted_on_either_side_of_the_subcommand() {
        // `global = true` is what makes both spellings equivalent, and an adapter
        // that appends its flags after the subcommand (the common case when the
        // subcommand is chosen first) must not have to know that.
        for argv in [
            vec![
                "processkit-cli",
                "--error-format",
                "json",
                "inspect",
                "--run-id",
                "build-42",
            ],
            vec![
                "processkit-cli",
                "inspect",
                "--run-id",
                "build-42",
                "--error-format",
                "json",
            ],
            vec![
                "processkit-cli",
                "inspect",
                "--error-format",
                "json",
                "--run-id",
                "build-42",
            ],
        ] {
            let cli = Cli::try_parse_from(&argv).unwrap_or_else(|err| panic!("{argv:?}: {err}"));
            assert_eq!(cli.error_format, ErrorFormat::Json, "{argv:?}");
            assert_eq!(cli.command.name(), "inspect");
            assert_eq!(cli.command.target_run_id(), Some("build-42"));
        }
    }

    #[test]
    fn every_subcommand_accepts_the_global_error_format() {
        // A global option that some subcommand silently rejected would be worse than
        // no option at all: `probe --json` advertises `<sub>:--error-format` for each
        // of these, and a consumer's fail-closed preflight would then pin a promise
        // the binary does not keep.
        for argv in [
            vec![
                "run",
                "--jsonl",
                "events.jsonl",
                "--error-format",
                "json",
                "--",
                "true",
            ],
            vec!["inspect", "--error-format", "json", "--run-id", "r1"],
            vec!["cancel", "--error-format", "json", "--run-id", "r1"],
            vec!["kill", "--error-format", "json", "--run-id", "r1"],
            vec!["attest", "--error-format", "json", "--run-id", "r1"],
            vec!["wait", "--error-format", "json", "--run-id", "r1"],
            vec!["events", "--error-format", "json", "--run-id", "r1"],
            vec!["list", "--error-format", "json"],
            vec!["prune", "--error-format", "json"],
            vec!["probe", "--json", "--error-format", "json"],
        ] {
            let mut full = vec!["processkit-cli"];
            full.extend(argv.iter().copied());
            let cli = Cli::try_parse_from(&full).unwrap_or_else(|err| panic!("{full:?}: {err}"));
            assert_eq!(cli.error_format, ErrorFormat::Json, "{full:?}");
        }
    }

    #[test]
    fn an_unknown_error_format_is_a_parse_time_refusal() {
        // Fail closed: a typo must not degrade to prose while the caller believes it
        // asked for machine output.
        assert!(
            Cli::try_parse_from(["processkit-cli", "--error-format", "yaml", "list"]).is_err(),
            "an unsupported format is refused rather than ignored"
        );
        assert!(
            Cli::try_parse_from(["processkit-cli", "list", "--error-format"]).is_err(),
            "the flag takes a value"
        );
    }

    #[test]
    fn a_commands_name_and_target_are_read_off_the_parsed_invocation() {
        // These two feed the envelope's `operation` and `run_id`; an aggregate or
        // whole-registry invocation names no single run and must report null rather
        // than invent one.
        for (argv, name, run_id) in [
            (vec!["list"], "list", None),
            (vec!["prune"], "prune", None),
            (vec!["probe", "--json"], "probe", None),
            (vec!["cancel", "--all"], "cancel", None),
            (vec!["kill", "--run-id", "r1"], "kill", Some("r1")),
            (vec!["attest", "--run-id", "r1"], "attest", Some("r1")),
            (vec!["wait", "--all"], "wait", None),
            (vec!["wait", "--run-id", "r2"], "wait", Some("r2")),
            (vec!["events", "--run-id", "r3"], "events", Some("r3")),
            (vec!["events", "--file", "e.jsonl"], "events", None),
            (vec!["inspect", "--all"], "inspect", None),
        ] {
            let mut full = vec!["processkit-cli"];
            full.extend(argv.iter().copied());
            let cli = Cli::try_parse_from(&full).unwrap_or_else(|err| panic!("{full:?}: {err}"));
            assert_eq!(cli.command.name(), name, "{full:?}");
            assert_eq!(cli.command.target_run_id(), run_id, "{full:?}");
        }

        let run = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--",
            "true",
        ])
        .expect("a run that names no id");
        assert_eq!(run.command.name(), "run");
        assert_eq!(
            run.command.target_run_id(),
            None,
            "a generated run id is minted inside the run and is not knowable here"
        );
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
                vec!["processkit-cli", "attest", "--run-id", bad],
                vec!["processkit-cli", "wait", "--run-id", bad],
                vec!["processkit-cli", "events", "--run-id", bad],
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
