//! Thin binary entry point for `processkit-cli`.
//!
//! All behavior lives in this crate's internal library (`processkit_cli`, see
//! `src/lib.rs`); this file only parses argv with clap and dispatches each
//! subcommand into that library. Keeping the binary thin lets the runner's
//! internals be exercised directly by the crate's unit/property/fuzz/bench tiers
//! through the library target, while the shipped binary — its CLI flags, exit
//! codes, and JSONL `schema_version` — remains the only supported compatibility
//! surface. (The `--error-format json` envelope is not a fourth: it rides on that
//! flag and on the reserved code it carries, pinning its own shape in the payload
//! with `error_version` — see `docs/compatibility.md`, "Machine-output schemas".)
//! The library is explicitly **not** a stable public Rust API; see the
//! library crate's own docs (`src/lib.rs`) for that disclaimer and the module map.

use std::process::ExitCode;

use clap::Parser;
use clap::error::ErrorKind;

use processkit_cli::cli::{Cli, Command, ErrorFormat};
use processkit_cli::error_envelope;
use processkit_cli::exit::{self, RunnerError};
use processkit_cli::{control, doctor, events_cmd, list, probe, prune, run, wait};

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return report_parse_error(err),
    };

    // The cross-argument rules clap cannot express declaratively — they compare two
    // flags' *values*, not their presence (see `Cli::validate`) — are checked here,
    // between parsing and dispatch. They report through the very same path a
    // malformed command line takes, so a violation is an ordinary `USAGE` (100)
    // refusal with clap's own rendering, decided before any registry entry, events
    // file, or child exists.
    if let Err(err) = cli.validate() {
        return report_parse_error(err);
    }

    // How a failure is *rendered* is decided once, here, from the one global option
    // (`--error-format`, accepted before or after the subcommand), and carried into
    // every reporting path below. What failed, and with which code, is decided
    // exactly as before: this choice changes stderr's shape and nothing else.
    let format = cli.error_format;
    let operation = cli.command.name();
    let target_run_id = cli.command.target_run_id().map(str::to_string);
    let run_id = target_run_id.as_deref();
    let report = |result| report_result(result, format, operation, run_id);

    // `run` owns the process's exit path: on a completed container it hard-exits
    // with the child's exact (full-width) code, so it never returns here. The one
    // exception is `run --detach`, which never becomes the runner at all — it
    // returns an ordinary `ExitCode` reporting whether the run *started* (see
    // `run::execute` and `docs/exit-codes.md`, "Detached runs"). Every
    // other subcommand either reaches a live runner over the control plane
    // (`inspect`/`cancel`/`kill`/`attest`), reads the per-user registry without contacting
    // any runner (`wait`/`list`/`prune`, and `events`, which also reads back the
    // run's own JSONL file), is entirely self-contained (`probe`), or drives a whole
    // scratch run of this very binary and then reports on it (`doctor`, the one
    // subcommand besides `run` that spawns anything) — and each
    // reports through the shared runner-error path below.
    match cli.command {
        Command::Run(args) => run::execute(*args, format),
        Command::Inspect(args) => report(if args.all {
            control::inspect_all(&args.labels, args.json)
        } else {
            control::inspect(
                args.run_id
                    .as_deref()
                    .expect("clap requires --run-id when --all is absent"),
                args.json,
            )
        }),
        Command::Cancel(args) => report(if args.all {
            control::cancel_all(&args.labels)
        } else {
            // clap's `required_unless_present`/`conflicts_with` pair on `TargetArgs`
            // guarantees exactly one of `run_id`/`all` is set, so this is never
            // reached with `run_id` absent.
            control::cancel(
                args.run_id
                    .as_deref()
                    .expect("clap requires --run-id when --all is absent"),
            )
        }),
        Command::Kill(args) => report(if args.all {
            control::kill_all(&args.labels)
        } else {
            control::kill(
                args.run_id
                    .as_deref()
                    .expect("clap requires --run-id when --all is absent"),
            )
        }),
        // `--run-id` is not an `Option` here: `attest` has no aggregate form, so
        // clap requires the single target outright (see `cli::AttestArgs`).
        Command::Attest(args) => report(control::attest(&args.run_id, args.json)),
        Command::Wait(args) => report(if args.all {
            wait::run_all(args.timeout, &args.labels, args.report_outcome)
        } else {
            // clap's `required_unless_present`/`conflicts_with` pair on `WaitArgs`
            // guarantees exactly one of `run_id`/`all` is set, so this is never
            // reached with `run_id` absent.
            wait::run(
                args.run_id
                    .as_deref()
                    .expect("clap requires --run-id when --all is absent"),
                args.timeout,
                args.report_outcome,
            )
        }),
        Command::Events(args) => report(events_cmd::run(&args)),
        Command::List(args) => report(list::run(args.json, &args.labels, args.health)),
        Command::Prune(args) => report(prune::run(args.json, args.dry_run, &args.labels)),
        Command::Probe(args) => report(probe::run(&args)),
        Command::Doctor(args) => report(doctor::run(&args)),
    }
}

/// Map a non-`run` command's result onto the process's exit code: success is
/// `0`, a runner-own failure reports itself on stderr and exits with its
/// reserved-band code (see `src/exit.rs` and `docs/exit-codes.md`).
///
/// *How* the failure is reported is the invocation's own choice: the historical
/// prose line by default, one bounded versioned JSON object under
/// `--error-format json`. Only stderr changes — the exit code is the same number
/// either way, and stdout is untouched, so a command that printed a JSON report
/// before failing (`probe --json`, `inspect --all --json`) still prints exactly what
/// it always did. The rendering itself lives in [`error_envelope::report_failure`],
/// shared with [`run::execute`]'s own exit path so the two cannot drift.
fn report_result(
    result: Result<(), RunnerError>,
    format: ErrorFormat,
    operation: &'static str,
    run_id: Option<&str>,
) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error_envelope::report_failure(&err, format, operation, run_id);
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
///
/// **These stay human-readable even under `--error-format json`, and that boundary
/// is deliberate** (`docs/exit-codes.md`, "What the envelope does not cover";
/// `docs/integration.md` §7). A parse error happens before this binary knows what it
/// was asked to do — there is no `operation` to name and no run to point at — and
/// clap's text is a usage/suggestion rendering for a human, not a verdict about a
/// run. A machine still learns everything the envelope would have told it from the
/// reserved `USAGE` (100) code, and `probe --require-surface`
/// (`docs/integration.md` §1) is the supported way to establish that a flag exists
/// *before* using it. Note that the value is not even reliably known here: an
/// invocation whose `--error-format` itself failed to parse has no format to honor.
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
