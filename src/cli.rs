//! Command-line surface for processkit-cli.
//!
//! This is the *CLI flags* half of the compatibility surface fixed by
//! `AGENTS.md`; the shapes here are normative and mirror README's "Planned
//! interface". Parsing and form validation are settled in this task; executing
//! each subcommand lands in later tasks (see docs/ROADMAP.md).

use std::ffi::OsString;
use std::path::PathBuf;

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
// The fields are parsed and validated in this task but not yet consumed — the
// runner that reads them lands in later tasks — so the binary crate would flag
// them as never-read without this allow.
#[allow(dead_code)]
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
    // Kept as an opaque string here: the duration grammar is not part of the
    // form fixed by this task and is parsed when timeouts are implemented.
    #[arg(long, value_name = "duration")]
    pub timeout: Option<String>,

    /// Grace period between a cancel/timeout and the hard kill.
    #[arg(long, value_name = "duration")]
    pub grace: Option<String>,

    /// Directory for bounded stdout/stderr capture files.
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
#[allow(dead_code)] // Parsed now, consumed once the control plane lands (see RunArgs).
#[derive(Debug, Args)]
pub struct InspectArgs {
    /// The run to inspect.
    #[arg(long, value_name = "id")]
    pub run_id: String,

    /// Emit the snapshot as JSON. Required to match the fixed form; JSON is
    /// currently the only supported output format.
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
}
