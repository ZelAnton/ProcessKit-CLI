//! Thin binary entry point for `processkit-cli`.
//!
//! All behavior lives in this crate's internal library (`processkit_cli`, see
//! `src/lib.rs`); this file only parses argv with clap and dispatches each
//! subcommand into that library. Keeping the binary thin lets the runner's
//! internals be exercised directly by the crate's unit/property/fuzz/bench tiers
//! through the library target, while the shipped binary — its CLI flags, exit
//! codes, and JSONL `schema_version` — remains the only supported compatibility
//! surface. The library is explicitly **not** a stable public Rust API; see the
//! library crate's own docs (`src/lib.rs`) for that disclaimer and the module map.

use std::process::ExitCode;

use clap::Parser;
use clap::error::ErrorKind;

use processkit_cli::cli::{Cli, Command};
use processkit_cli::exit::{self, RunnerError};
use processkit_cli::{control, list, probe, prune, run};

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
        Command::Run(args) => run::execute(*args),
        Command::Inspect(args) => report(control::inspect(&args.run_id)),
        Command::Cancel(args) => report(control::cancel(&args.run_id)),
        Command::Kill(args) => report(control::kill(&args.run_id)),
        Command::List(args) => report(list::run(args.json)),
        Command::Prune(args) => report(prune::run(args.json)),
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
