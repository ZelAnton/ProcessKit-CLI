//! processkit-cli — run one shell-free command inside ProcessKit's containment
//! boundary and report its lifecycle.
//!
//! The `run` subcommand is implemented here (see [`run`]): it spawns the child
//! into a ProcessKit container this process owns, echoes the child's output
//! live, forwards its exit code faithfully, and writes the versioned JSONL
//! lifecycle events (see [`events`] and `docs/schema.md`) to the `--jsonl` file.
//! The control plane's first client, `inspect`, is implemented in [`control`]: it
//! reaches a live `run` over the per-user registry and local transport and prints a
//! machine-readable snapshot. `cancel`/`kill` still report a runner-range "not
//! implemented" error (T-009) rather than panicking or exiting 0. The compatibility
//! surface — CLI flags (see [`cli`]), the exit-code contract (see [`exit`] and
//! `docs/exit-codes.md`), and the JSONL `schema_version` (see [`events`] and
//! `docs/schema.md`) — is fixed.

mod capture;
mod cli;
mod control;
mod events;
mod exit;
mod hash;
mod registry;
mod run;

use std::process::ExitCode;

use clap::Parser;
use clap::error::ErrorKind;

use cli::{Cli, Command};
use exit::RunnerError;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return report_parse_error(err),
    };

    // `run` owns the process's exit path: on a completed container it hard-exits
    // with the child's exact (full-width) code, so it never returns here. The
    // remaining subcommands share the runner-error reporting below.
    match cli.command {
        Command::Run(args) => run::execute(args),
        other => match dispatch(other) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("processkit-cli: {err}");
                ExitCode::from(err.code())
            }
        },
    }
}

/// Map clap's parse outcome onto the runner exit-code contract. `--help` and
/// `--version` are successful requests: clap has already formatted their text
/// for stdout, so we print it and exit 0. Every genuine parse failure exits with
/// the runner-own [`exit::USAGE`] code instead of clap's default `2`, keeping the
/// runner's failures inside its documented band.
fn report_parse_error(err: clap::Error) -> ExitCode {
    let _ = err.print();
    match err.kind() {
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayVersion
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => ExitCode::SUCCESS,
        _ => ExitCode::from(exit::USAGE),
    }
}

/// Route a control-plane command to its handler. `inspect` reaches a live runner
/// through the registry and local transport (see [`control::inspect`]);
/// `cancel`/`kill` are still stubs (T-009), each returning a runner-range "not
/// implemented" error so that task can replace them without touching the exit-code
/// contract. `run` is handled directly in [`main`] and never reaches here.
fn dispatch(command: Command) -> Result<(), RunnerError> {
    match command {
        Command::Run(_) => Err(RunnerError::not_implemented("run")),
        Command::Inspect(args) => control::inspect(&args.run_id),
        Command::Cancel(_) => Err(RunnerError::not_implemented("cancel")),
        Command::Kill(_) => Err(RunnerError::not_implemented("kill")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn binary_name_is_stable() {
        assert_eq!(env!("CARGO_PKG_NAME"), "processkit-cli");
    }
}
