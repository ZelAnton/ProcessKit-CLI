//! Arguments for the `events` subcommand (the reader itself lives in
//! [`crate::events_cmd`] — the plain `events` module name was already taken by the
//! JSONL *emitter* this command reads back, see [`crate::events`]).

use std::path::PathBuf;

use clap::Args;

use super::parse::parse_run_id;

/// `events (--run-id <id> | --file <path>) [--json | --validate] [--follow]`
///
/// Reads back the versioned JSONL lifecycle stream a run wrote with `run --jsonl`
/// (`docs/schema.md`) and renders it, passes it through, or checks it — closing the
/// detach → observe → conclude loop inside the tool instead of leaving a supervisor
/// to hand-roll `tail`/`jq` recipes around a locator it discovered with
/// `list --json` (see [`crate::events_cmd`] and `docs/detached-runs.md`).
///
/// Read-only in the same sense `list`/`wait` are: it opens the per-user registry
/// through [`crate::registry::Registry::open_read_only`] to resolve (and, under
/// `--follow`, to keep watching) the locator, never mutates registry state, and never
/// connects to any run's control transport — so reading a run's story cannot disturb,
/// end, or even be noticed by the run.
///
/// # Naming the stream
///
/// `--run-id` and `--file` are the two ways to name one stream, they are mutually
/// exclusive (`conflicts_with`, the same clap convention `WaitArgs`'s modes use), and
/// exactly one is required (`required_unless_present`): a bare `events` names no
/// stream and is rejected at parse time as a `USAGE` (100) form error.
///
/// Deliberately **no precedence rule** for the two together. A "`--file` silently
/// wins over `--run-id`" rule would make a caller that passes both — most plausibly
/// by mistake, e.g. a template that always appends `--file` — read a *different*
/// stream than the one it named, with nothing said about it. Rejecting the
/// combination structurally at the clap level, the way
/// [`crate::cli::ProbeArgs::print_schema`] rejects composing with `--require-*`
/// (K-076), keeps the ambiguity impossible rather than merely resolved.
#[derive(Debug, Args)]
pub struct EventsArgs {
    /// Resolve the stream through the per-user registry: the JSONL locator published
    /// by the record(s) registered under this run id (`list --json`'s `jsonl` field).
    /// Works for a live run *and* for a finished-but-not-yet-reaped one, whose record
    /// still names the completed stream. Mutually exclusive with `--file`; exactly
    /// one of the two is required.
    #[arg(
        long,
        value_name = "id",
        value_parser = parse_run_id,
        conflicts_with = "file",
        required_unless_present = "file"
    )]
    pub run_id: Option<String>,

    /// Read this events file directly, skipping registry resolution entirely — the
    /// escape hatch for a stream whose registry record is already gone (a clean exit
    /// deletes its own record, and `prune` reaps the leftovers of one that did not),
    /// and the way to check an events file that was never registered here at all,
    /// such as an adapter's own fixture. Mutually exclusive with `--run-id`; exactly
    /// one of the two is required.
    #[arg(
        long,
        value_name = "path",
        conflicts_with = "run_id",
        required_unless_present = "run_id"
    )]
    pub file: Option<PathBuf>,

    /// Pass each stream line through **verbatim** instead of rendering it — the raw
    /// JSONL bytes the runner wrote, byte for byte, never re-serialized through this
    /// binary's own event types (see [`crate::events_cmd`], "Why `--json` is a
    /// pass-through"). Mutually exclusive with `--validate`, which replaces the event
    /// output with a conformance report rather than reshaping it.
    #[arg(long, conflicts_with = "validate")]
    pub json: bool,

    /// Keep reading as the stream grows, until the terminal `runner_exit` event is
    /// observed — the detached-run counterpart to watching a foreground run's own
    /// output.
    ///
    /// Bounded by the run's own lifetime rather than by an invented deadline: a
    /// follow also returns, with an explanation on stderr, once the registry says the
    /// run is over and the stream has stopped growing — which is what an abruptly
    /// killed runner (one that never got to write its terminal event) leaves behind.
    /// It never blocks past the end of the run it is watching. See
    /// [`crate::events_cmd`], "Following, and when it stops".
    #[arg(long)]
    pub follow: bool,

    /// Check every line against the JSON Schema this binary embeds at build time —
    /// the very document `probe --print-schema` prints — instead of printing the
    /// events: each non-conforming line is reported with its line number and what it
    /// violated, then a summary.
    ///
    /// Exits `0` when every line conforms and [`crate::exit::EVENTS_INVALID`] (114)
    /// when any does not, so an adapter author can gate their own fixtures on it in
    /// CI, and tell "invalid" apart from "could not be checked" (an unreadable stream
    /// is still `SETUP`, 111). Mutually exclusive with `--json`.
    #[arg(long)]
    pub validate: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    use crate::cli::{Cli, Command};

    fn events_args(extra: &[&str]) -> EventsArgs {
        let mut argv = vec!["processkit-cli", "events"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv).expect("a valid events invocation");
        let Command::Events(args) = cli.command else {
            panic!("expected the events subcommand");
        };
        args
    }

    #[test]
    fn events_requires_exactly_one_locator() {
        let args = events_args(&["--run-id", "build-42"]);
        assert_eq!(args.run_id.as_deref(), Some("build-42"));
        assert!(args.file.is_none());
        assert!(!args.json && !args.follow && !args.validate);

        let args = events_args(&["--file", "run.jsonl"]);
        assert_eq!(
            args.file.as_deref(),
            Some(std::path::Path::new("run.jsonl"))
        );
        assert!(args.run_id.is_none());

        assert!(
            Cli::try_parse_from(["processkit-cli", "events"]).is_err(),
            "a bare events names no stream and must be rejected at parse time"
        );
        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "events",
                "--run-id",
                "r1",
                "--file",
                "run.jsonl"
            ])
            .is_err(),
            "the two locators are mutually exclusive: no silent precedence rule"
        );
    }

    /// `--json` (pass through the raw lines) and `--validate` (replace the events with
    /// a conformance report) are two different answers to "what goes on stdout", so
    /// clap refuses the combination outright rather than letting one silently win —
    /// the same structural rejection `probe --print-schema` uses (K-076).
    #[test]
    fn json_and_validate_are_mutually_exclusive_at_the_clap_level() {
        assert!(events_args(&["--file", "run.jsonl", "--json"]).json);
        assert!(events_args(&["--file", "run.jsonl", "--validate"]).validate);
        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "events",
                "--file",
                "run.jsonl",
                "--json",
                "--validate"
            ])
            .is_err(),
            "a report-replacing mode must not compose with the pass-through mode"
        );
    }

    /// `--follow` is a modifier, not a mode: it composes with every output form.
    #[test]
    fn follow_composes_with_every_output_mode() {
        assert!(events_args(&["--run-id", "r1", "--follow"]).follow);
        let args = events_args(&["--run-id", "r1", "--follow", "--json"]);
        assert!(args.follow && args.json);
        let args = events_args(&["--file", "run.jsonl", "--follow", "--validate"]);
        assert!(args.follow && args.validate);
    }

    /// The run id goes through the shared [`parse_run_id`] — the same ingress bar
    /// every other by-id command applies — so a value that could reshape a terminal
    /// line is a `USAGE` form error here too, never something this command renders.
    #[test]
    fn events_rejects_unsafe_run_ids_at_parse_time() {
        let too_long = "x".repeat(257);
        for bad in ["", "line\nbreak", "bidi\u{202e}override", &too_long] {
            assert!(
                Cli::try_parse_from(["processkit-cli", "events", "--run-id", bad]).is_err(),
                "events must reject {bad:?} like every other by-id command"
            );
        }
    }
}
