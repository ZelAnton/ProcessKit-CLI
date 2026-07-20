//! The `run` subcommand: launch one shell-free program inside a ProcessKit
//! container, echo its output live, forward its exit code faithfully, and bound
//! the run with a hard `--timeout` and an interactive `Ctrl-C` cancel.
//!
//! This is the first executable path of the runner (see `docs/ROADMAP.md`,
//! "Runnable containment shell"). It builds strictly on the public `processkit`
//! API — the single source of truth for containment and teardown — and never
//! reimplements any of it (`AGENTS.md`, "Build strictly on the public
//! `processkit` API"). Four settled decisions are realized here:
//!
//! - **Own the group.** The child is spawned into a [`ProcessGroup`] this module
//!   owns, not a shared/global one, so the group's kernel-backed kill-on-drop —
//!   a Windows Job Object close, a Linux cgroup/POSIX-group teardown — reaps the
//!   whole tree (including any leaked grandchild) when the group drops, on every
//!   exit path. The teardown is the group's, never a hand-rolled wait/cleanup
//!   loop on top of it. The group is dropped only *after* the outcome is decided.
//! - **Live output is pipe + echo, not fd inheritance.** processkit's line pump
//!   reads the child's stdout/stderr and tees each line to *our* stdout/stderr.
//!   The child therefore sees no TTY (documented in `README.md`: colors and
//!   progress bars may degrade). Streams stay strictly separated — child stdout
//!   to our stdout, child stderr to our stderr — and no runner diagnostic is ever
//!   written to the child's stdout.
//! - **Exit-code fidelity, with distinguishable runner-imposed endings.** On a
//!   completed run the process exits with the child's *exact* code (full width,
//!   never clamped). When the runner instead *ends* the run — the `--timeout`
//!   deadline elapsed, or the operator pressed `Ctrl-C` — the child did not choose
//!   to stop, so its code is not forwarded: the run reports a reserved-band code
//!   ([`exit::TIMEOUT`] / [`exit::CANCELLED`]) and an explanatory stderr line, kept
//!   distinct from each other and from any child result. (Their machine-readable
//!   JSONL form lands with the event schema — see `docs/ROADMAP.md` §2 / T-004.)
//! - **One teardown path for every ending, honest per platform.** The deadline
//!   and the cancel share a single termination path: attempt a *soft* stop
//!   (`SIGTERM` to the whole tree on Unix), wait out `--grace`, then let the owning
//!   group's kill-on-drop hard-tear-down the tree. On **Windows** there is no
//!   soft-signal tier in the ProcessKit kernel yet (tracked in ProcessKit-rs's
//!   backlog), so no soft signal is sent — the grace window still elapses and the
//!   Job Object is then killed atomically. The runner never *pretends* a soft stop
//!   happened when it could not: the stderr message states exactly what the
//!   platform did (see [`describe_teardown`]).

use std::process::ExitCode;
use std::time::Duration;

use processkit::{Command as PkCommand, Error as PkError, Outcome, ProcessGroup, Signal};

use crate::cli::RunArgs;
use crate::exit::{self, RunnerError};

/// Execute the `run` subcommand and turn the result into a process exit code.
///
/// On a completed container the child's code is forwarded verbatim via
/// [`std::process::exit`], which preserves the full 32-bit width (a Windows code
/// such as `STATUS_CONTROL_C_EXIT` is not clamped to a `u8`). That hard exit
/// skips destructors, which is *only* safe because the container has already been
/// torn down inside [`run_inner`] — the owning [`ProcessGroup`] drops before this
/// function regains control. A runner-own failure (including a `--timeout` or a
/// `Ctrl-C` cancel) instead reports to stderr (never the child's stdout) and
/// returns a code from the reserved band.
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
    // output pumps, a deadline timer, and a Ctrl-C listener. `enable_all` arms the
    // I/O, time, and signal drivers those need — the child-pipe I/O driver is
    // compiled in through `processkit`'s own tokio `process`/`net` features, and
    // the `time`/`signal` features this crate now requests arm the rest (Cargo
    // unifies them into the single tokio build).
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

/// How a run ended — the decision the race in [`run_async`] resolves to.
enum Ending {
    /// The child exited on its own; carries the raw wait result.
    Exited(processkit::Result<Outcome>),
    /// The `--timeout` deadline elapsed while the child was still running.
    TimedOut,
    /// The operator pressed `Ctrl-C`.
    Cancelled,
}

/// A runner-imposed ending — the child did not exit on its own.
enum Termination {
    /// The `--timeout` deadline (the elapsed limit) was exceeded.
    Timeout(Duration),
    /// The run was cancelled interactively (`Ctrl-C`).
    Cancelled,
}

/// What the *soft* stop actually did, recorded so the outcome is reported
/// honestly rather than by assumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoftTerminate {
    /// A real soft signal (`SIGTERM`) was delivered to the whole tree (Unix).
    Signalled,
    /// The platform has no soft-terminate tier yet (Windows): nothing was sent,
    /// and we do not claim otherwise.
    Unsupported,
    /// The soft signal could not be delivered; the run falls through to the hard
    /// kill regardless.
    Failed,
}

/// Own a group, spawn the child into it, stream its output live, and report how
/// the run ended. The group drops when this future completes — on success or on
/// any error path — which is what tears the container down.
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
    // leaves teardown with us (`kills_tree_on_drop()` is `false`). The deadline and
    // the cancel are *not* handed to the child handle (via `Command::timeout` /
    // `cancel_on`); we run the race ourselves so both endings share one grace +
    // kill-on-drop teardown, exactly as this task requires.
    let running = group.start(&command).await.map_err(map_launch_error)?;

    let timeout = args.timeout;
    let grace = args.grace;

    // Race the child's own exit against the two runner-imposed endings. Whichever
    // fires first *decides* the outcome; only then does teardown begin, so the
    // owning group is never dropped before the outcome is known.
    //
    // `biased` order — cancel, natural exit, deadline — makes the tie-breaks
    // deliberate: a `Ctrl-C` always wins, and a child that exits in the very poll
    // its deadline fires is reported as its own exit rather than a timeout. When
    // the cancel/deadline branch wins, `running` (moved into `wait()`) is dropped;
    // because this is a shared-group handle that does not kill on drop, the child
    // stays alive for the grace path below, and its output pumps stop (teardown is
    // underway).
    let ending = tokio::select! {
        biased;
        () = wait_for_ctrl_c() => Ending::Cancelled,
        outcome = running.wait() => Ending::Exited(outcome),
        () = deadline(timeout) => Ending::TimedOut,
    };

    match ending {
        Ending::Exited(outcome) => {
            let outcome = outcome.map_err(|err| {
                RunnerError::new(
                    exit::INTERNAL,
                    format!("waiting for the child to exit failed: {err}"),
                )
            })?;
            exit_code_for(outcome)
            // `group` drops here → the container, including any leaked grandchild,
            // is torn down before we return the child's code.
        }
        Ending::TimedOut => {
            let limit = timeout.expect("the deadline arm only fires when --timeout is set");
            let soft = soft_terminate_then_grace(&group, grace).await;
            Err(termination_error(Termination::Timeout(limit), soft, grace))
            // `group` drops here → kill-on-drop hard-tears-down any survivor.
        }
        Ending::Cancelled => {
            let soft = soft_terminate_then_grace(&group, grace).await;
            Err(termination_error(Termination::Cancelled, soft, grace))
            // `group` drops here → kill-on-drop hard-tears-down any survivor.
        }
    }
}

/// The runner-imposed deadline: sleep `limit`, or (with no `--timeout`) never
/// resolve, so the race falls through to the other arms.
async fn deadline(limit: Option<Duration>) {
    match limit {
        Some(limit) => tokio::time::sleep(limit).await,
        None => std::future::pending::<()>().await,
    }
}

/// Resolve when the operator presses `Ctrl-C`. If the signal handler cannot be
/// installed we degrade to "no cancel" (never resolving) after an honest warning,
/// rather than aborting an otherwise-healthy run.
async fn wait_for_ctrl_c() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => {}
        Err(err) => {
            eprintln!("processkit-cli: warning: Ctrl-C handling is unavailable: {err}");
            std::future::pending::<()>().await;
        }
    }
}

/// The shared teardown path for both runner-imposed endings: try a soft stop,
/// wait out `--grace`, and report what the soft stop actually did. The *hard*
/// teardown is not done here — the caller drops the owning [`ProcessGroup`]
/// afterwards, and its kernel-backed kill-on-drop is the single hard-kill path.
///
/// On Unix the soft stop is a `SIGTERM` broadcast to the whole tree. On Windows
/// [`ProcessGroup::signal`] supports only `Signal::Kill`, so a `SIGTERM` request
/// returns [`PkError::Unsupported`]: no soft signal is delivered, and we record
/// that faithfully instead of pretending. Either way the grace window still
/// elapses (giving a child that *can* stop — e.g. one that received the console's
/// own `Ctrl-C` on Windows — a chance to exit first) before the atomic kill.
async fn soft_terminate_then_grace(group: &ProcessGroup, grace: Option<Duration>) -> SoftTerminate {
    let soft = match group.signal(Signal::Term) {
        Ok(()) => SoftTerminate::Signalled,
        Err(PkError::Unsupported { .. }) => SoftTerminate::Unsupported,
        // Best-effort: a delivery failure does not stop teardown — the group's
        // kill-on-drop still reaps the tree — but it is reported honestly.
        Err(_) => SoftTerminate::Failed,
    };
    if let Some(grace) = grace {
        tokio::time::sleep(grace).await;
    }
    soft
}

/// Turn a runner-imposed ending into the reserved-band error it surfaces:
/// [`exit::TIMEOUT`] / [`exit::CANCELLED`] plus a message that names the ending
/// and describes, truthfully, how the tree was torn down.
fn termination_error(
    kind: Termination,
    soft: SoftTerminate,
    grace: Option<Duration>,
) -> RunnerError {
    let (code, headline) = match kind {
        Termination::Timeout(limit) => (
            exit::TIMEOUT,
            format!("run timed out after {}", format_duration(limit)),
        ),
        Termination::Cancelled => (exit::CANCELLED, "run cancelled (Ctrl-C)".to_string()),
    };
    RunnerError::new(
        code,
        format!("{headline}: {}", describe_teardown(soft, grace)),
    )
}

/// A truthful, human-readable description of the teardown that just happened —
/// the load-bearing part of the "honest degradation" contract. It states whether
/// a real soft signal was delivered, whether a grace window was waited, and that
/// the hard kill is the container's kill-on-drop (a Windows Job Object terminate).
fn describe_teardown(soft: SoftTerminate, grace: Option<Duration>) -> String {
    let waited = match grace {
        Some(grace) => format!("waited {} grace, then ", format_duration(grace)),
        None => String::new(),
    };
    match soft {
        SoftTerminate::Signalled => format!(
            "sent SIGTERM to the process tree, {waited}hard-killed it via the container's kill-on-drop"
        ),
        SoftTerminate::Unsupported => format!(
            "Windows has no soft-terminate signal yet, so — after {}— the process tree was \
             hard-killed atomically via the Job Object",
            match grace {
                Some(grace) => format!("a {} grace delay ", format_duration(grace)),
                None => "no grace delay ".to_string(),
            }
        ),
        SoftTerminate::Failed => format!(
            "the soft-terminate signal could not be delivered, so {waited}the process tree was \
             hard-killed via the container's kill-on-drop"
        ),
    }
}

/// A compact, honest rendering of a duration for diagnostics: whole seconds when
/// it divides evenly (`5s`), otherwise milliseconds (`500ms`). Not a full
/// human-time formatter — just enough to echo the deadline/grace back clearly.
fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms != 0 && ms.is_multiple_of(1_000) {
        format!("{}s", ms / 1_000)
    } else {
        format!("{ms}ms")
    }
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
/// a child code. A `TimedOut` outcome cannot occur here: the runner arms no
/// `Command::timeout` (the `--timeout` deadline is raced in [`run_async`] and
/// reported as [`exit::TIMEOUT`] instead), so a `TimedOut` from the backend is an
/// invariant violation rather than a result.
fn exit_code_for(outcome: Outcome) -> Result<i32, RunnerError> {
    match outcome {
        Outcome::Exited(code) => Ok(code),
        Outcome::Signalled(Some(signal)) => Ok(128 + (signal & 0x7f)),
        Outcome::Signalled(None) => Ok(128),
        Outcome::TimedOut => Err(RunnerError::new(
            exit::INTERNAL,
            "the run reported a timeout, but no deadline was armed on the child",
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

    #[test]
    fn timeout_and_cancel_carry_distinct_reserved_codes() {
        let timed_out = termination_error(
            Termination::Timeout(Duration::from_secs(5)),
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        let cancelled = termination_error(
            Termination::Cancelled,
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        assert_eq!(timed_out.code(), exit::TIMEOUT);
        assert_eq!(cancelled.code(), exit::CANCELLED);
        assert_ne!(timed_out.code(), cancelled.code());
    }

    #[test]
    fn timeout_message_names_the_ending_and_the_limit() {
        let err = termination_error(
            Termination::Timeout(Duration::from_secs(5)),
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        let msg = err.to_string();
        assert!(
            msg.contains("timed out"),
            "message should name the timeout: {msg}"
        );
        assert!(msg.contains("5s"), "message should echo the limit: {msg}");
    }

    #[test]
    fn cancel_message_names_ctrl_c() {
        let err = termination_error(Termination::Cancelled, SoftTerminate::Signalled, None);
        let msg = err.to_string();
        assert!(
            msg.contains("cancelled"),
            "message should say cancelled: {msg}"
        );
        assert!(msg.contains("Ctrl-C"), "message should name Ctrl-C: {msg}");
    }

    #[test]
    fn unix_teardown_reports_a_real_soft_signal_and_the_grace() {
        // Where the soft path exists, the message states the SIGTERM was sent and
        // the grace was waited — no "Windows"/"Job Object" wording.
        let msg = describe_teardown(SoftTerminate::Signalled, Some(Duration::from_secs(2)));
        assert!(msg.contains("SIGTERM"), "{msg}");
        assert!(msg.contains("2s"), "{msg}");
        assert!(msg.contains("grace"), "{msg}");
        assert!(!msg.contains("Windows"), "{msg}");
    }

    #[test]
    fn windows_teardown_is_reported_honestly_without_pretending() {
        // The "honest degradation" contract: when no soft signal could be sent,
        // the message says so plainly and names the atomic Job Object kill — it
        // must never imply a graceful soft-terminate was performed.
        let msg = describe_teardown(SoftTerminate::Unsupported, Some(Duration::from_secs(2)));
        assert!(msg.contains("Windows"), "{msg}");
        assert!(msg.contains("Job Object"), "{msg}");
        assert!(msg.contains("no soft-terminate"), "{msg}");
        assert!(
            !msg.contains("sent SIGTERM"),
            "must not claim a soft signal was delivered: {msg}"
        );
    }

    #[test]
    fn teardown_without_grace_omits_the_grace_wording() {
        let msg = describe_teardown(SoftTerminate::Signalled, None);
        assert!(msg.contains("SIGTERM"), "{msg}");
        assert!(!msg.contains("grace"), "no grace was configured: {msg}");
    }

    #[test]
    fn failed_soft_terminate_is_reported_but_still_hard_kills() {
        let msg = describe_teardown(SoftTerminate::Failed, Some(Duration::from_secs(1)));
        assert!(msg.contains("could not be delivered"), "{msg}");
        assert!(msg.contains("hard-killed"), "{msg}");
    }

    #[test]
    fn format_duration_is_compact_and_honest() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_millis(500)), "500ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1500ms");
        assert_eq!(format_duration(Duration::ZERO), "0ms");
    }
}
