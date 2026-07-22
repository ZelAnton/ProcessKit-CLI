//! processkit-cli — run one shell-free command inside ProcessKit's containment
//! boundary and report its lifecycle.
//!
//! The `run` subcommand is implemented here (see [`run`]): it spawns the child
//! into a ProcessKit container this process owns, echoes the child's output
//! live, forwards its exit code faithfully, and writes the versioned JSONL
//! lifecycle events (see [`events`] and `docs/schema.md`) to the `--jsonl` file.
//! The control plane's clients live in [`control`]: `inspect` reaches a live `run`
//! over the per-user registry and local transport and prints a machine-readable
//! snapshot, and `cancel`/`kill` reach the same live runner over the same transport
//! to end it — a graceful soft-stop → grace → hard-kill for `cancel`, an immediate
//! hard kill for `kill` — each a distinguishable outcome in the JSONL stream and by
//! exit code. [`list`] is the discovery counterpart: it scans the same registry and
//! prints every entry, live or stale, for a caller that has lost (or never had) a
//! `run_id`. The compatibility surface — CLI flags (see [`cli`]), the exit-code
//! contract (see [`exit`] and `docs/exit-codes.md`), and the JSONL `schema_version`
//! (see [`events`] and `docs/schema.md`) — is fixed.

mod capture;
mod cli;
mod control;
mod events;
mod exit;
mod hash;
mod list;
mod probe;
mod registry;
mod run;
#[cfg(windows)]
mod win_security;

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
    // with the child's exact (full-width) code, so it never returns here. Every
    // other subcommand reaches a live runner (`inspect`/`cancel`/`kill`) or is
    // self-contained (`probe`) and reports through the shared runner-error path
    // below.
    match cli.command {
        Command::Run(args) => run::execute(args),
        Command::Inspect(args) => report(control::inspect(&args.run_id)),
        Command::Cancel(args) => report(control::cancel(&args.run_id)),
        Command::Kill(args) => report(control::kill(&args.run_id)),
        Command::List(args) => report(list::run(args.json)),
        Command::Probe(args) => report(probe::run(&args)),
    }
}

/// Map a non-`run` command's result onto the process's exit code: success is
/// `0`, a runner-own failure prints its message to stderr and exits with its
/// reserved-band code (see `src/exit.rs` and `docs/exit-codes.md`).
fn report(result: Result<(), RunnerError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("processkit-cli: {err}");
            ExitCode::from(err.code())
        }
    }
}

/// Map clap's parse outcome onto the runner exit-code contract. `--help` and
/// `--version` are successful requests: clap has already formatted their text
/// for stdout, so we print it and exit 0. Every genuine parse failure — including
/// a bare invocation with no subcommand at all (clap's
/// `DisplayHelpOnMissingArgumentOrSubcommand`) — exits with the runner-own
/// [`exit::USAGE`] code instead of clap's default `2`, keeping the runner's
/// failures inside its documented band and failing loudly rather than reporting
/// success for an invalid command line.
fn report_parse_error(err: clap::Error) -> ExitCode {
    let _ = err.print();
    match err.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => ExitCode::SUCCESS,
        _ => ExitCode::from(exit::USAGE),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn binary_name_is_stable() {
        assert_eq!(env!("CARGO_PKG_NAME"), "processkit-cli");
    }
}
