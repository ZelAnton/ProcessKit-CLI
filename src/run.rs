//! The `run` subcommand: launch one shell-free program inside a ProcessKit
//! container, echo its output live, and forward its exit code faithfully.
//!
//! This is the first executable path of the runner (see `docs/ROADMAP.md`,
//! "Runnable containment shell"). It builds strictly on the public `processkit`
//! API — the single source of truth for containment and teardown — and never
//! reimplements any of it (`AGENTS.md`, "Build strictly on the public
//! `processkit` API"). Three settled decisions are realized here:
//!
//! - **Own the group.** The child is spawned into a [`ProcessGroup`] this module
//!   owns, not a shared/global one, so the group's kernel-backed kill-on-drop —
//!   a Windows Job Object close, a Linux cgroup/POSIX-group teardown — reaps the
//!   whole tree (including any leaked grandchild) when the group drops, on every
//!   exit path. The teardown is the group's, never a hand-rolled wait/cleanup
//!   loop on top of it.
//! - **Live output is pipe + echo, not fd inheritance.** processkit's line pump
//!   reads the child's stdout/stderr and tees each line to *our* stdout/stderr.
//!   The child therefore sees no TTY (documented in `README.md`: colors and
//!   progress bars may degrade). Streams stay strictly separated — child stdout
//!   to our stdout, child stderr to our stderr — and no runner diagnostic is ever
//!   written to the child's stdout.
//! - **Exit-code fidelity.** On a completed run the process exits with the
//!   child's *exact* code (full width, never clamped); the runner's own failures
//!   use the reserved `100..=119` band (see [`crate::exit`]).

use std::process::ExitCode;

use processkit::{Command as PkCommand, Error as PkError, Outcome, ProcessGroup};

use crate::cli::RunArgs;
use crate::exit::{self, RunnerError};

/// Execute the `run` subcommand and turn the result into a process exit code.
///
/// On a completed container the child's code is forwarded verbatim via
/// [`std::process::exit`], which preserves the full 32-bit width (a Windows code
/// such as `STATUS_CONTROL_C_EXIT` is not clamped to a `u8`). That hard exit
/// skips destructors, which is *only* safe because the container has already been
/// torn down inside [`run_inner`] — the owning [`ProcessGroup`] drops before this
/// function regains control. A runner-own failure instead reports to stderr
/// (never the child's stdout) and returns a code from the reserved band.
pub fn execute(args: RunArgs) -> ExitCode {
    match run_inner(args) {
        Ok(child_code) => std::process::exit(child_code),
        Err(err) => {
            eprintln!("processkit-cli: {err}");
            ExitCode::from(err.code())
        }
    }
}

/// Build the async runtime and drive one run to its exit code.
///
/// The runtime and the container both live for the duration of [`run_async`];
/// when it returns the group has already dropped (teardown done), so the caller
/// may hard-exit with the child's code.
fn run_inner(args: RunArgs) -> Result<i32, RunnerError> {
    // A small current-thread runtime is enough: the run is one child plus its
    // output pumps. `enable_all` arms the I/O and time drivers the pumps need —
    // the child-pipe I/O driver is compiled in through `processkit`'s own tokio
    // `process`/`net` features (Cargo unifies them into the single tokio build).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| {
            RunnerError::new(
                exit::INTERNAL,
                format!("could not start the async runtime: {err}"),
            )
        })?;
    runtime.block_on(run_async(args))
}

/// Own a group, spawn the child into it, stream its output live, and report the
/// child's exit code. The group drops when this future completes — on success or
/// on any error path — which is what tears the container down.
async fn run_async(args: RunArgs) -> Result<i32, RunnerError> {
    // clap guarantees at least one token (`num_args = 1..`, `required = true`).
    let (program, program_args) = args
        .command
        .split_first()
        .expect("clap enforces a non-empty command after `--`");

    // We own this group; the child — and anything it spawns — is a member. When
    // `group` drops at the end of this scope (every path below), the kernel reaps
    // the whole tree. Containment/teardown is the group's job; we never duplicate
    // it (`AGENTS.md`, "Never clean up by process name").
    let group = ProcessGroup::new().map_err(|err| {
        RunnerError::new(
            exit::BACKEND,
            format!("could not create the ProcessKit container: {err}"),
        )
    })?;

    let mut command = PkCommand::new(program).args(program_args);
    // Default cwd is the runner's own current directory (processkit leaves it
    // unset), so only override when `--cwd` was given.
    if let Some(cwd) = &args.cwd {
        command = command.current_dir(cwd);
    }
    // `--create-no-window` maps straight onto `Command::create_no_window()`
    // (`CREATE_NO_WINDOW` on Windows, a no-op elsewhere). Default: OFF. A bare
    // `run` should behave like launching the child directly, so we do not force
    // the flag — that would diverge from a direct launch and could hide a child
    // that legitimately wants its own console. Headless Windows deployments
    // (Orchestra) that must avoid a stray `conhost` window pass the flag
    // explicitly; the runner itself never allocates a console, so it spawns no
    // extra host on its own account. (See README, "Windows console".)
    if args.create_no_window {
        command = command.create_no_window();
    }
    // Pipe + echo: the pump reads the child's piped stdout/stderr and tees each
    // decoded line to our own stdout/stderr. This is the live-output mechanism —
    // not true fd inheritance — so the child gets no TTY, and the two streams are
    // never crossed or mixed with runner diagnostics.
    command = command
        .stdout_tee(tokio::io::stdout())
        .stderr_tee(tokio::io::stderr());

    // `ProcessGroup::start` joins the child to the group *we* own and hands back a
    // handle that deliberately does not own the group, so dropping the handle
    // leaves teardown with us.
    let running = group.start(&command).await.map_err(map_launch_error)?;

    // `wait` drives the root to exit and discards its output (already teed live).
    // It consumes the handle; only `group` still gates teardown afterwards.
    let outcome = running.wait().await.map_err(|err| {
        RunnerError::new(
            exit::INTERNAL,
            format!("waiting for the child to exit failed: {err}"),
        )
    })?;

    exit_code_for(outcome)
    // `group` drops here → the container, including any leaked grandchild, is
    // torn down before we return the child's code.
}

/// Map a `processkit` launch failure onto the runner-own exit-code band.
///
/// A locate/start failure is [`exit::SPAWN`] — the child never ran; every other
/// backend/containment failure is [`exit::BACKEND`]. A child's own exit is never
/// routed through here (it is an [`Outcome`], not an [`Err`]).
fn map_launch_error(err: PkError) -> RunnerError {
    match err {
        PkError::NotFound { .. } | PkError::Spawn { .. } => {
            RunnerError::new(exit::SPAWN, format!("could not start the program: {err}"))
        }
        other => RunnerError::new(
            exit::BACKEND,
            format!("ProcessKit backend failure: {other}"),
        ),
    }
}

/// Derive the process exit code from a completed run's [`Outcome`].
///
/// A clean exit forwards the child's code untouched. A signal death — Unix only;
/// Windows reports [`Outcome::Exited`] even for `Ctrl-C` — has no code of its
/// own, so it is rendered as `128 + signo`, the POSIX shell convention. That sits
/// above the runner-own band, so it can never be mistaken for a runner failure or
/// a child code. A `TimedOut` outcome cannot occur here (no `--timeout` is armed
/// in this task) and is treated as an invariant violation rather than a result.
fn exit_code_for(outcome: Outcome) -> Result<i32, RunnerError> {
    match outcome {
        Outcome::Exited(code) => Ok(code),
        Outcome::Signalled(Some(signal)) => Ok(128 + (signal & 0x7f)),
        Outcome::Signalled(None) => Ok(128),
        Outcome::TimedOut => Err(RunnerError::new(
            exit::INTERNAL,
            "the run reported a timeout, but no deadline was configured",
        )),
        // `Outcome` is `#[non_exhaustive]`; a variant this build predates cannot
        // be faithfully rendered as a child code, so report a runner fault rather
        // than guess at one.
        _ => Err(RunnerError::new(
            exit::INTERNAL,
            "the run produced an outcome this build does not recognize",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exited_code_is_forwarded_verbatim() {
        assert_eq!(exit_code_for(Outcome::Exited(0)).unwrap(), 0);
        assert_eq!(exit_code_for(Outcome::Exited(7)).unwrap(), 7);
        // Full-width Windows codes survive: no clamp to a u8.
        assert_eq!(
            exit_code_for(Outcome::Exited(-1073741510)).unwrap(),
            -1073741510
        );
    }

    #[test]
    fn signal_death_uses_the_posix_convention() {
        // 128 + SIGKILL(9), 128 + SIGTERM(15).
        assert_eq!(exit_code_for(Outcome::Signalled(Some(9))).unwrap(), 137);
        assert_eq!(exit_code_for(Outcome::Signalled(Some(15))).unwrap(), 143);
        assert_eq!(exit_code_for(Outcome::Signalled(None)).unwrap(), 128);
    }

    #[test]
    fn a_timeout_without_a_deadline_is_a_runner_fault() {
        let err = exit_code_for(Outcome::TimedOut).unwrap_err();
        assert_eq!(err.code(), exit::INTERNAL);
    }

    #[test]
    fn other_backend_failures_map_to_the_backend_code() {
        // `NotFound`/`Spawn` are `#[non_exhaustive]`, so they cannot be built
        // here; the SPAWN mapping is proved through the binary instead (running a
        // program that does not exist — see `tests/run.rs`). Every remaining
        // launch failure lands on the BACKEND code.
        let io = map_launch_error(PkError::Io(std::io::Error::from(
            std::io::ErrorKind::AddrInUse,
        )));
        assert_eq!(io.code(), exit::BACKEND);
    }
}
