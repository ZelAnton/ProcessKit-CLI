//! Command-line surface for processkit-cli.
//!
//! This is the *CLI flags* half of the compatibility surface fixed by
//! `AGENTS.md`; the shapes here are normative and mirror README's "Planned
//! interface". Parsing and form validation are settled in this task; executing
//! each subcommand lands in later tasks (see docs/ROADMAP.md).

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

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

/// The four commands that make up the runner's control surface.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a program inside a ProcessKit container and report its lifecycle.
    Run(RunArgs),
    /// Query a live run over local IPC.
    Inspect(InspectArgs),
    /// Ask a live run to cancel (graceful where supported, then a hard kill).
    Cancel(TargetArgs),
    /// Hard-kill a live run's container immediately.
    Kill(TargetArgs),
}

/// `run [--run-id <id>] [--cwd <dir>] --jsonl <events.jsonl> [--create-no-window]
/// [--timeout <duration>] [--grace <duration>] [--capture-dir <dir>] [--argv-raw]
/// -- <program> <args...>`
//
// `run` consumes every field: `cwd`, `create_no_window`, `timeout`, `grace`,
// `command`, `jsonl`, `run_id`, `argv_raw` (the JSONL schema, T-004), and now
// `capture_dir` — bounded stdout/stderr capture to files (see `src/capture.rs`).
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Identifier for this run; a value is generated when omitted.
    #[arg(long, value_name = "id")]
    pub run_id: Option<String>,

    /// Working directory for the child process.
    #[arg(long, value_name = "dir")]
    pub cwd: Option<PathBuf>,

    /// File to receive the versioned JSONL lifecycle events (never stdout).
    #[arg(long, value_name = "events.jsonl")]
    pub jsonl: PathBuf,

    /// Windows: create the child with CREATE_NO_WINDOW.
    #[arg(long)]
    pub create_no_window: bool,

    /// Hard deadline for the whole run; the tree is torn down when it elapses.
    // Parsed and validated *here*, at the CLI layer, rather than deferred to the
    // runner: a malformed duration is a form error like any other bad flag, so it
    // belongs with the parsing surface (this module) and surfaces as the same
    // documented `USAGE` (100) exit, not a mid-run failure. `run` then receives an
    // already-validated `Duration` and never re-parses a string. See
    // [`parse_duration`] for the accepted grammar.
    #[arg(long, value_name = "duration", value_parser = parse_duration)]
    pub timeout: Option<Duration>,

    /// Grace period between a cancel/timeout and the hard kill. Same grammar and
    /// parse-time validation as `--timeout` (see [`parse_duration`]).
    #[arg(long, value_name = "duration", value_parser = parse_duration)]
    pub grace: Option<Duration>,

    /// Directory for bounded stdout/stderr capture files (`stdout.log`,
    /// `stderr.log`). When set, the child's output is teed into these files
    /// alongside the live echo; each stream's byte count, content hash, and
    /// truncation flag are reported in the `output_captured` JSONL event.
    #[arg(long, value_name = "dir")]
    pub capture_dir: Option<PathBuf>,

    /// Record the raw argv in diagnostics instead of the redacted hash + hint.
    #[arg(long)]
    pub argv_raw: bool,

    /// The program to run followed by its arguments. Everything after `--` is
    /// taken verbatim — there is no shell mode, so nothing here is expanded or
    /// re-interpreted. Kept as `OsString`s to preserve bytes exactly.
    #[arg(last = true, required = true, num_args = 1.., value_name = "program")]
    pub command: Vec<OsString>,
}

/// `inspect --run-id <id> --json`
#[derive(Debug, Args)]
pub struct InspectArgs {
    /// The run to inspect.
    #[arg(long, value_name = "id")]
    pub run_id: String,

    /// Emit the snapshot as JSON. Required to match the fixed form; JSON is
    /// currently the only supported output format, so the flag is not branched on —
    /// clap enforces its presence and `inspect` always prints JSON.
    #[allow(dead_code)] // Part of the fixed CLI form; enforced by clap, never read.
    #[arg(long, required = true)]
    pub json: bool,
}

/// Shared argument for the by-run-id control commands (`cancel`, `kill`).
#[allow(dead_code)] // Parsed now, consumed once the control plane lands (see RunArgs).
#[derive(Debug, Args)]
pub struct TargetArgs {
    /// The run to act on.
    #[arg(long, value_name = "id")]
    pub run_id: String,
}

/// Parse a human duration for `--timeout` / `--grace`.
///
/// Grammar: a base-10, non-negative integer with an optional unit suffix — `ms`,
/// `s` (the default when the suffix is omitted), `m`, or `h`. Examples: `30`
/// (= 30 seconds), `500ms`, `5s`, `2m`, `1h`. Deliberately strict — a sign, a
/// fraction, surrounding whitespace, or an unknown unit is rejected rather than
/// silently reinterpreted, so a typo fails loudly at parse time instead of arming
/// a surprising deadline. The value is capped only by `u64` milliseconds; an
/// overflow is reported, not wrapped.
///
/// Returns the message that clap renders on failure (which the binary maps to the
/// `USAGE` exit code); on success it hands `run` a ready `Duration`.
fn parse_duration(raw: &str) -> Result<Duration, String> {
    if raw.is_empty() {
        return Err("empty duration; expected e.g. `30`, `500ms`, `5s`, `2m`, or `1h`".to_string());
    }

    // Split the leading digit run from the unit suffix. A value that does not
    // start with a digit (a sign, a bare unit, letters) leaves `number` empty.
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (number, unit) = raw.split_at(split);
    if number.is_empty() {
        return Err(format!(
            "duration `{raw}` must start with a non-negative number; \
             expected e.g. `30`, `500ms`, `5s`, `2m`, or `1h`"
        ));
    }

    let value: u64 = number
        .parse()
        .map_err(|_| format!("duration `{raw}` is out of range for a 64-bit millisecond count"))?;

    let millis = match unit {
        "" | "s" => value.checked_mul(1_000),
        "ms" => Some(value),
        "m" => value.checked_mul(60_000),
        "h" => value.checked_mul(3_600_000),
        other => {
            return Err(format!(
                "duration `{raw}` has an unknown unit `{other}`; use ms, s, m, or h"
            ));
        }
    };

    let millis = millis
        .ok_or_else(|| format!("duration `{raw}` is too large to represent in milliseconds"))?;
    Ok(Duration::from_millis(millis))
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
    fn run_captures_the_command_verbatim_after_double_dash() {
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--",
            "cmd",
            "/c",
            "--not-a-runner-flag",
            "echo hi",
        ])
        .expect("a valid run invocation");

        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert_eq!(args.jsonl, PathBuf::from("events.jsonl"));
        assert!(args.run_id.is_none(), "--run-id is optional for run");
        // Flags after `--` must survive as literal argv, not be parsed as runner
        // options.
        assert_eq!(
            args.command,
            vec![
                OsString::from("cmd"),
                OsString::from("/c"),
                OsString::from("--not-a-runner-flag"),
                OsString::from("echo hi"),
            ]
        );
    }

    #[test]
    fn run_requires_jsonl_and_a_command() {
        assert!(
            Cli::try_parse_from(["processkit-cli", "run", "--", "cmd"]).is_err(),
            "--jsonl is required"
        );
        assert!(
            Cli::try_parse_from(["processkit-cli", "run", "--jsonl", "e.jsonl"]).is_err(),
            "a command after `--` is required"
        );
    }

    #[test]
    fn inspect_requires_run_id_and_json() {
        assert!(
            Cli::try_parse_from(["processkit-cli", "inspect", "--run-id", "r1", "--json"]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["processkit-cli", "inspect", "--run-id", "r1"]).is_err(),
            "--json is part of the fixed form"
        );
        assert!(
            Cli::try_parse_from(["processkit-cli", "inspect", "--json"]).is_err(),
            "--run-id is required"
        );
    }

    #[test]
    fn cancel_and_kill_require_a_run_id() {
        assert!(Cli::try_parse_from(["processkit-cli", "cancel", "--run-id", "r1"]).is_ok());
        assert!(Cli::try_parse_from(["processkit-cli", "kill", "--run-id", "r1"]).is_ok());
        assert!(Cli::try_parse_from(["processkit-cli", "cancel"]).is_err());
        assert!(Cli::try_parse_from(["processkit-cli", "kill"]).is_err());
    }

    #[test]
    fn parse_duration_accepts_the_documented_grammar() {
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn parse_duration_rejects_malformed_values() {
        // Empty, non-numeric, signed, fractional, unknown unit, and whitespace all
        // fail loudly rather than being silently reinterpreted.
        for bad in ["", "abc", "-5", "5x", "1.5s", "s", "5 s", " 5s", "ms"] {
            assert!(
                parse_duration(bad).is_err(),
                "expected `{bad}` to be rejected as a duration"
            );
        }
    }

    #[test]
    fn parse_duration_reports_overflow_instead_of_wrapping() {
        // A value that would overflow the millisecond count is an error, never a
        // wrapped-around tiny duration.
        assert!(parse_duration("99999999999999999999h").is_err());
        assert!(parse_duration(&format!("{}h", u64::MAX)).is_err());
    }

    #[test]
    fn run_parses_timeout_and_grace_into_durations() {
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--timeout",
            "5s",
            "--grace",
            "500ms",
            "--",
            "true",
        ])
        .expect("a valid run invocation");
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert_eq!(args.timeout, Some(Duration::from_secs(5)));
        assert_eq!(args.grace, Some(Duration::from_millis(500)));
    }

    #[test]
    fn run_rejects_a_malformed_timeout() {
        // A bad duration is a form error, so parsing fails (mapped to USAGE by the
        // binary) rather than reaching the runner.
        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "run",
                "--jsonl",
                "events.jsonl",
                "--timeout",
                "soon",
                "--",
                "true",
            ])
            .is_err(),
            "a malformed --timeout must fail at parse time"
        );
    }
}
