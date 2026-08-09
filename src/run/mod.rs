//! The `run` subcommand: launch one shell-free program inside a ProcessKit
//! container, route its output live, forward its exit code faithfully, and bound
//! the run with a hard `--timeout` and a local stop-signal cancel (`Ctrl-C`, on
//! Unix `SIGTERM`/`SIGHUP`, and on Windows `Ctrl-Break`/console close/logoff/
//! system shutdown).
//!
//! This is the first executable path of the runner (see `docs/ROADMAP.md`,
//! "Runnable containment shell"). It builds strictly on the public `processkit`
//! API — the single source of truth for containment and teardown — and never
//! reimplements any of it (`AGENTS.md`, "Build strictly on the public
//! `processkit` API"). Five settled decisions are realized here:
//!
//! - **Own the group.** The child is spawned into a [`processkit::ProcessGroup`] this module
//!   owns, not a shared/global one, so the group's kernel-backed kill-on-drop —
//!   a Windows Job Object close, a Linux cgroup/POSIX-group teardown — reaps the
//!   whole tree (including any leaked grandchild) when the group drops, on every
//!   exit path. The teardown is the group's, never a hand-rolled wait/cleanup
//!   loop on top of it. The group is dropped only *after* the outcome is decided.
//! - **Output is pipe + echo by default, direct inheritance by opt-in.** The
//!   default path uses processkit's line pump and therefore exposes no TTY to the
//!   child. `--inherit-stdio` instead maps all three streams onto ProcessKit's
//!   public inheritance modes, preserving the caller's terminal handles without
//!   a runner-side pump. Streams stay strictly separated either way, and no runner
//!   diagnostic is ever written to the child's stdout.
//! - **Exit-code fidelity, with distinguishable runner-imposed endings.** On a
//!   completed run the process exits with the child's *exact* code (full width,
//!   never clamped). When the runner instead *ends* the run — the `--timeout`
//!   deadline elapsed, a local stop signal arrived (`Ctrl-C`, on Unix `SIGTERM` /
//!   `SIGHUP`, or on Windows `Ctrl-Break`/console close/logoff/system shutdown),
//!   or a control-plane
//!   `cancel`/`kill` command reached the live runner — the child did not choose to
//!   stop, so its code is not forwarded: the run reports a reserved-band code
//!   ([`crate::exit::TIMEOUT`] / [`crate::exit::CANCELLED`] /
//!   [`crate::exit::CONTROL_CANCELLED`] / [`crate::exit::CONTROL_KILLED`] /
//!   [`crate::exit::OUTPUT_OVERFLOW`]) and an
//!   explanatory stderr line, kept distinct from
//!   each other and from any child result. Their machine-readable JSONL form is the
//!   `timeout` / `output_overflow` / `cancelled` / `killed` (plus terminal
//!   `runner_exit`) event written
//!   to `--jsonl` (see [`crate::events`] and `docs/schema.md`). The control-plane
//!   endings reuse the *same* teardown as the local ones — `cancel` runs the shared
//!   soft-stop → grace → hard-kill path, `kill` hard-kills the tree at once — so a
//!   remote command never invents a parallel termination mechanism.
//! - **One teardown path for every ending, honest per platform.** The deadline
//!   and the cancel share ProcessKit's single reporting stop path: capability
//!   probe, soft request, grace, and hard escalation. On Unix the request is
//!   `SIGTERM` to the tree. On **Windows** a Job Object has no POSIX signal, so the
//!   soft tier is `WM_CLOSE` to windowed members plus `CTRL_BREAK` for an opted-in
//!   console leader. A tree with neither reports a `none` capability and escalates
//!   atomically. The runner never
//!   *pretends* a soft stop happened when it could not, and never claims a signal
//!   the platform cannot send: the stderr message states exactly what was done
//!   (see [`teardown::describe_teardown`]).
//! - **Detaching wraps the run; it never forks a second implementation of it.**
//!   `--detach` re-spawns *this binary* on the very same argv (minus the flag) in
//!   a new session (Unix) or as a `DETACHED_PROCESS` (Windows), waits until that
//!   copy has provably started the run, and returns. The detached copy then walks
//!   the ordinary path above — same container, same race, same teardown, same
//!   JSONL — so detaching adds a spawn plus a handshake and nothing else (see
//!   [`start_detached`]).

mod detach;
mod launch;
mod signals;
mod teardown;

#[doc(hidden)]
pub use launch::parse_env_file_contents;

// The four ProcessKit vocabularies this module projects onto the JSONL wire,
// re-exported so one caller can reach every projection point by name: the upstream
// identifier drift gate (`tests/spec_drift.rs`, the `spec-drift` feature tier),
// which resolves each identifier in ProcessKit's shipped `spec/identifiers.json`
// back to its variant and runs it through these exact functions instead of a
// re-derived lookalike table of its own. `events::mechanism_str`,
// `events::abrupt_cleanup_scope_str`, and `events::outcome_fields` are the same
// surface for the vocabularies that module owns.
#[doc(hidden)]
pub use launch::limit_kind_str;
#[doc(hidden)]
pub use teardown::{limit_verdict_str, soft_signal_str, soft_stop_scope_str};

use std::process::ExitCode;
use std::time::Duration;

use processkit::Outcome;

use crate::capture::CaptureOverflow;
use crate::cli::{ErrorFormat, RunArgs};
use crate::control;
use crate::error_envelope;
use crate::exit::RunnerError;

use detach::start_detached;
use launch::run_async;

/// Execute the `run` subcommand and turn the result into a process exit code.
///
/// On a completed container the child's code is forwarded verbatim via
/// [`std::process::exit`], which preserves the full 32-bit width (a Windows code
/// such as `STATUS_CONTROL_C_EXIT` is not clamped to a `u8`). That hard exit
/// skips destructors, which is *only* safe because the container has already been
/// torn down inside [`run_inner`] — the owning [`processkit::ProcessGroup`] drops before this
/// function regains control. A runner-own failure (including a `--timeout` or a
/// `Ctrl-C` cancel) instead reports to stderr (never the child's stdout) and
/// returns a code from the reserved band.
///
/// **`--detach` short-circuits all of that**, and is the one path where this
/// process never becomes the runner at all: it hands the run to a detached copy of
/// this binary and reports only whether that copy *started* — `0` on a confirmed
/// start, the same reserved-band code the failed start reported otherwise. No child
/// code is ever forwarded on this path (there is no child of ours to forward one
/// from); it lives on in the detached run's own `runner_exit` event. See
/// [`start_detached`].
///
/// `format` is the invocation's global `--error-format`, and it governs **only** the
/// shape of the stderr line a runner-own failure prints (prose, or the bounded JSON
/// envelope — see [`crate::error_envelope`]). It changes no exit code, nothing on
/// stdout, and none of the child's own output; the `processkit-cli: warning: …`
/// lines this run may emit along the way are not failures and stay prose either way.
/// A failing `run` is also the one place the envelope restates something the JSONL
/// stream already carries: its `kind` is spelled exactly like the terminal
/// `runner_exit` event's `source`, deliberately reusing that vocabulary rather than
/// forking it, and it is the account available when a run was started without
/// `--jsonl` (or, with `--detach`, never got far enough to write one).
pub fn execute(args: RunArgs, format: ErrorFormat) -> ExitCode {
    // The id the *invocation* named, not the one the runner may generate: an
    // envelope reports what the caller can correlate against (see
    // `cli::Command::target_run_id`).
    let run_id = args.run_id.clone();
    let report = |err: &RunnerError| {
        error_envelope::report_failure(err, format, "run", run_id.as_deref());
        ExitCode::from(err.code())
    };
    if args.detach {
        return match start_detached(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => report(&err),
        };
    }
    match run_inner(args) {
        Ok(child_code) => std::process::exit(child_code),
        Err(err) => report(&err),
    }
}

/// Build the async runtime and drive one run to its exit code.
///
/// The runtime and the container both live for the duration of [`run_async`];
/// when it returns the group has already dropped (teardown done), so the caller
/// may hard-exit with the child's code.
fn run_inner(args: RunArgs) -> Result<i32, RunnerError> {
    // A small current-thread runtime is enough: the run is one child plus its
    // output pumps, a deadline timer, and the stop-signal listeners (`Ctrl-C`, plus
    // `SIGTERM`/`SIGHUP` on Unix, plus `Ctrl-Break`/console close/logoff/system
    // shutdown on Windows). The shared helper's
    // `enable_all` arms the I/O, time, and signal drivers those need — the
    // child-pipe I/O driver is compiled in through `processkit`'s own tokio
    // `process`/`net` features, and the `time`/`signal` features this crate now
    // requests arm the rest (Cargo unifies them into the single tokio build).
    let runtime = control::current_thread_runtime()?;
    runtime.block_on(run_async(args))
}

/// Which runner deadline fired, for the shared timeout ending — the two share the
/// reserved `TIMEOUT` (106) code and the same teardown, told apart only by this tag
/// (surfaced as the `timeout` event's `reason` field, `docs/schema.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutTrigger {
    /// The whole-run `--timeout` deadline elapsed.
    Overall,
    /// The `--idle-timeout` elapsed: the child produced no output for the idle window.
    Idle,
}

impl TimeoutTrigger {
    /// The `timeout` event's always-present `reason` value for this trigger.
    fn reason(self) -> &'static str {
        match self {
            TimeoutTrigger::Overall => "overall",
            TimeoutTrigger::Idle => "idle",
        }
    }
}

/// What asked for a `members_snapshot` — the honest `reason` of that event, and the
/// only thing telling the snapshot every run emits after spawn apart from an opt-in
/// periodic re-sample.
///
/// **Decision (T-298): repeated `members_snapshot` events are additive within schema
/// v1, and they say so on the wire.** Two questions had to be answered before the
/// `--snapshot-interval` cadence could exist at all:
///
/// - *Does repeating an existing event break v1?* No. `schema_version` versions each
///   event's **shape** (`docs/schema.md`, "Versioning"): a breaking change renames or
///   removes a field, changes a field's type, or changes the meaning of a value. A
///   second `members_snapshot` line does none of those — every field keeps its name,
///   type, and meaning — and the normative ordering prose was already written for
///   readers that route by event type. What the ordering section did *not* previously
///   state was the **multiplicity**, so it now says so explicitly rather than leaving
///   "exactly one" to be inferred from a sentence that never claimed it. The cadence
///   is opt-in and off by default besides: a run without the flag emits the same
///   number of snapshots, at the same point, as before.
/// - *Should a repeat be distinguishable?* Yes, and by the same convention every
///   other multi-trigger event in this stream already follows — `timeout.reason`
///   (`overall`/`idle`), `cancelled.source`, `container_failed.phase`,
///   `runner_exit.source`: when one event type can arise from more than one trigger,
///   the event names its own trigger instead of leaving a consumer to infer it from
///   position in the stream. Adding an always-present field to an existing event is
///   the additive change `docs/schema.md`'s "Versioning" section explicitly blesses
///   (the `timeout` event's own `reason` was added exactly this way), at the cost of
///   regenerating that event's golden fixture line (K-049).
///
/// **Decision (T-298, review R-04): a failed sample is reported in the stream, not
/// only on stderr.** `emit_members_snapshot` originally warned on the runner's stderr
/// and skipped the event entirely when `ProcessGroup::members_info()` failed. That
/// degradation is unobservable in the cadence's own headline scenario: a detached
/// runner is spawned with `stdin`/`stdout`/`stderr` set to `Stdio::null()`
/// (`detach::spawn_detached`), so its stderr reaches nobody, and the JSONL file —
/// the only artifact such a run has — showed a failed sample as *nothing at all*,
/// indistinguishable from a tree that simply had not changed. An observability
/// feature that cannot report its own failure to its main audience is not honest, so
/// the event is now always emitted, carrying an always-present `read_error` flag with
/// an empty `members` fallback on failure.
///
/// This follows the project's own established precedent rather than inventing one:
/// `cleanup_started.read_error` and `cleanup_finished.read_error` already qualify a
/// fallback count/PID list exactly this way instead of letting it pass as a confirmed
/// observation (`teardown::snapshot_members_or_unknown` is the third sibling of
/// `members_len_or_unknown`/`remaining_pids_or_unknown`). It is additive within schema
/// v1 on the same terms as `reason` above — a new always-present field on an existing
/// event — and it makes the ordering contract stricter rather than looser: the
/// post-spawn `members_snapshot` now really does appear exactly once in every stream,
/// as `docs/schema.md`'s "Ordering" section asserts, where before a failed read
/// silently removed it.
///
/// The stderr warning is kept for a foreground operator, but is deliberately *not*
/// the contract any more — `docs/schema.md`, `docs/detached-runs.md`, and the
/// `--snapshot-interval` help text all state that it is absent under `--detach` and
/// that `read_error` is the channel that always works. What remains outside this
/// event's reach is a JSONL write failure itself: `Emitter::poison` disables logging
/// after one stderr warning, so a full disk (or any unwritable `--jsonl`) truncates
/// the stream rather than annotating it. That boundary is documented where an
/// operator meets it (`docs/running-commands.md`, "Recorded tree snapshots"), because
/// no event can report the failure of the channel that would carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotReason {
    /// The one snapshot every run emits, immediately after `run_started`.
    Spawn,
    /// A periodic re-sample armed by `run --snapshot-interval`.
    Interval,
}

impl SnapshotReason {
    /// The `members_snapshot` event's always-present `reason` value for this
    /// trigger (`docs/schema.md`, "members_snapshot").
    fn reason(self) -> &'static str {
        match self {
            SnapshotReason::Spawn => "spawn",
            SnapshotReason::Interval => "interval",
        }
    }
}

/// Which **local stop signal** asked the runner to end the run — the honest
/// `source` of the `cancelled` JSONL event and the trigger the stderr line names.
///
/// **Decision (T-188): SIGTERM and SIGHUP get their own additive `source` values**
/// (`sigterm` / `sighup`) rather than reusing `ctrl_c`. Reusing `ctrl_c` for a
/// `systemd stop`, a cancelled CI job, or a plain `kill <pid>` would report a
/// keyboard interrupt that never happened — the same lie the runner refuses to tell
/// about a soft stop it could not deliver (see
/// [`SoftTerminate`]/[`teardown::describe_teardown`]),
/// and consumers do act on the difference: "the operator interrupted me" and "my
/// supervisor is shutting me down" call for different handling. Adding values to an
/// existing string field is an **additive** schema change (no `schema_version` bump,
/// see `docs/schema.md`, "Versioning"), so the cost is one enum entry per echo site.
///
/// **Decision (T-195): the Windows console-control events get the same additive
/// treatment** (`ctrl_break` / `ctrl_close` / `ctrl_logoff` / `ctrl_shutdown`), for
/// the identical reason — each is a distinguishable *external* trigger, not a
/// keyboard interrupt, and a consumer that only knows `ctrl_c` still sees a
/// well-formed `cancelled` event.
///
/// The exit code is *not* split the same way: every local-signal cancel keeps
/// [`crate::exit::CANCELLED`] (107) and the `cancelled` terminal `runner_exit` source,
/// because it is the same class of ending (a local signal ended the run) and the more
/// specific `cancelled.source` already disambiguates it one event earlier — the same
/// reasoning that kept `--idle-timeout` on `TIMEOUT` (106) rather than minting a code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelSignal {
    /// The operator pressed `Ctrl-C` (`SIGINT` on Unix, the console handler on
    /// Windows).
    CtrlC,
    /// Unix `SIGTERM`: the standard *external* stop — `kill <pid>`, `systemctl stop`,
    /// a cancelled CI job, a supervisor's shutdown timeout. Not an interactive
    /// interrupt, and the most common way a runner is asked to go away.
    #[cfg(unix)]
    Term,
    /// Unix `SIGHUP`: the controlling terminal went away (a closed terminal, a dropped
    /// SSH session). Treated as a stop, not as the daemon "reload your config"
    /// convention — this runner supervises exactly one child and has nothing to reload,
    /// and the default disposition would kill it outright anyway.
    #[cfg(unix)]
    Hup,
    /// Windows `CTRL_BREAK_EVENT`: the operator (or a script) sent a break to the
    /// console process group. Unlike the other three Windows events below, this one
    /// carries no OS-imposed termination deadline — a process that ignores it simply
    /// keeps running — so it needs no grace clamp.
    #[cfg(windows)]
    CtrlBreak,
    /// Windows `CTRL_CLOSE_EVENT`: the console window is being closed (its "X"
    /// button, or an equivalent). The OS gives the handler only a short window
    /// (documented at `signals::CTRL_CLOSE_WINDOW`) before terminating the process
    /// regardless — see [`signals::effective_grace_for`] for how that bounds this trigger's
    /// effective `--grace`.
    #[cfg(windows)]
    CtrlClose,
    /// Windows `CTRL_LOGOFF_EVENT`: the user is logging off. Not delivered to a
    /// process outside the logging-off user's own session.
    #[cfg(windows)]
    CtrlLogoff,
    /// Windows `CTRL_SHUTDOWN_EVENT`: the system is shutting down.
    #[cfg(windows)]
    CtrlShutdown,
}

impl CancelSignal {
    /// The `cancelled` event's `source` value for this trigger (`docs/schema.md`,
    /// "cancelled").
    fn source(self) -> &'static str {
        match self {
            CancelSignal::CtrlC => "ctrl_c",
            #[cfg(unix)]
            CancelSignal::Term => "sigterm",
            #[cfg(unix)]
            CancelSignal::Hup => "sighup",
            #[cfg(windows)]
            CancelSignal::CtrlBreak => "ctrl_break",
            #[cfg(windows)]
            CancelSignal::CtrlClose => "ctrl_close",
            #[cfg(windows)]
            CancelSignal::CtrlLogoff => "ctrl_logoff",
            #[cfg(windows)]
            CancelSignal::CtrlShutdown => "ctrl_shutdown",
        }
    }

    /// How the stderr line names this trigger to a human.
    fn phrase(self) -> &'static str {
        match self {
            CancelSignal::CtrlC => "Ctrl-C",
            #[cfg(unix)]
            CancelSignal::Term => "SIGTERM",
            #[cfg(unix)]
            CancelSignal::Hup => "SIGHUP",
            #[cfg(windows)]
            CancelSignal::CtrlBreak => "Ctrl-Break",
            #[cfg(windows)]
            CancelSignal::CtrlClose => "console close",
            #[cfg(windows)]
            CancelSignal::CtrlLogoff => "logoff",
            #[cfg(windows)]
            CancelSignal::CtrlShutdown => "system shutdown",
        }
    }
}

/// How a run ended — the decision the race in [`run_async`] resolves to.
enum Ending {
    /// The child exited on its own; carries the raw wait result.
    Exited(processkit::Result<Outcome>),
    /// A runner deadline elapsed while the child was still running: the whole-run
    /// `--timeout` ([`TimeoutTrigger::Overall`]) or the `--idle-timeout`
    /// ([`TimeoutTrigger::Idle`]). Both take the same teardown and terminal code.
    TimedOut(TimeoutTrigger),
    /// A capture stream exceeded its byte ceiling while the opt-in `cancel`
    /// overflow policy was active.
    OutputOverflow(CaptureOverflow),
    /// A local stop signal reached the runner — `Ctrl-C`, (Unix) `SIGTERM` /
    /// `SIGHUP`, or (Windows) `Ctrl-Break`/console close/logoff/system shutdown.
    /// All take the same teardown and terminal code; the carried
    /// [`CancelSignal`] is what tells them apart on the wire.
    Cancelled(CancelSignal),
    /// A control-plane `cancel` command reached the live runner: the same soft-stop →
    /// grace → hard-kill teardown as `Ctrl-C`, only triggered over the network.
    ControlCancelled,
    /// A control-plane `kill` command reached the live runner: an immediate hard kill
    /// of the whole tree, no soft stop and no grace.
    ControlKilled,
}

/// A runner-imposed ending that shares the soft-stop → grace → hard-kill teardown
/// (the `kill` verb is *not* one — it hard-kills immediately, handled separately).
enum Termination {
    /// A runner deadline (the elapsed `limit`) was exceeded: `trigger` names which —
    /// the whole-run `--timeout` or the `--idle-timeout`.
    Timeout {
        limit: Duration,
        trigger: TimeoutTrigger,
    },
    /// A bounded capture stream exceeded its configured ceiling.
    OutputOverflow(CaptureOverflow),
    /// The run was cancelled by a local stop signal: `Ctrl-C`, (Unix) `SIGTERM` /
    /// `SIGHUP`, or (Windows) `Ctrl-Break`/console close/logoff/system shutdown.
    /// The carried [`CancelSignal`] names which, so the message stays honest.
    Cancelled(CancelSignal),
    /// The run was cancelled by a control-plane `cancel` command.
    ControlCancelled,
}

/// What the *soft* stop actually did, recorded so the outcome is reported
/// honestly rather than by assumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoftTerminate {
    /// A soft stop really was delivered to the tree: a `SIGTERM` broadcast on
    /// Unix, or — since ProcessKit 3 — a best-effort soft close on Windows (a
    /// `WM_CLOSE` to a windowed member; a Job Object has no POSIX signal).
    Signalled,
    /// Nothing in the tree could receive a soft stop, so none was delivered and
    /// we do not claim otherwise. Windows-only in practice: a Job Object with no
    /// windowed member and no opt-in console-CTRL leader has nothing ProcessKit's
    /// soft tier can trigger. Every Unix backend always has a real `SIGTERM`
    /// tier.
    Unsupported,
    /// The soft signal could not be delivered; the run falls through to the hard
    /// kill regardless.
    Failed,
}
