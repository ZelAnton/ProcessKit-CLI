//! processkit-cli — run one shell-free command inside ProcessKit's containment
//! boundary and report its lifecycle.
//!
//! This task fixes two thirds of the project's compatibility surface: the CLI
//! flags (see [`cli`]) and the runner exit-code contract (see [`exit`] and
//! `docs/exit-codes.md`). The runner itself — spawning, IPC, and the JSONL
//! schema — is implemented in later tasks; every subcommand currently reports a
//! runner-range "not implemented" error rather than panicking or exiting 0.

mod cli;
mod exit;

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

    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("processkit-cli: {err}");
            ExitCode::from(err.code())
        }
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

/// Route a parsed command to its handler. Every path is unimplemented in this
/// task; each returns a runner-range "not implemented" error so downstream tasks
/// can replace the stub without touching the exit-code contract.
fn dispatch(cli: Cli) -> Result<(), RunnerError> {
    match cli.command {
        Command::Run(_) => Err(RunnerError::not_implemented("run")),
        Command::Inspect(_) => Err(RunnerError::not_implemented("inspect")),
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
