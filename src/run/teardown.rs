//! How a run ends: the shared teardown tiers, the JSONL event emitters, and the
//! mapping from a backend failure or a decided ending onto the reserved
//! runner-own exit-code band.
//!
//! Two teardown tiers, and one site each, so no caller can drift from the
//! others: [`emit_hard_teardown`] for every ending with no soft-stop tier of its
//! own (a natural exit, a wait failure, a control-plane `kill`), and
//! [`graceful_teardown`] — ProcessKit's reported soft stop, grace, and escalation —
//! for the three that have one (`timeout`, a signal `cancel`, `control_cancel`).
//! Both end at the same terminal
//! [`finish`], which writes the one [`Event::RunnerExit`] every return path of
//! [`super::launch::run_async`] owes the stream.
//!
//! The emitters here follow one convention throughout: a container read that
//! fails is reported as *not confirmed* (`read_error`), never fabricated as a
//! confirmed-empty or confirmed-clean tree — the same honest-degradation contract
//! [`describe_teardown`] applies to a soft stop the platform could not deliver.

use std::time::Duration;

use processkit::{
    Error as PkError, ErrorReason as PkErrorReason, Outcome, ProcessGroup, SoftSignal,
};

use crate::capture::Capture;
use crate::duration_fmt::format_duration;
use crate::events::{Emitter, Event, Member, ShutdownInfo};
use crate::exit::{self, RunnerError};
use crate::registry;

use super::{SoftTerminate, Termination, TimeoutTrigger};

/// Emit the terminal [`Event::RunnerExit`] for a runner-own failure and return the
/// error unchanged, so each failing path reads as one expression. `source` names
/// the ending and `child_code` carries the child's own code when one exists (it is
/// `None` for every runner-own failure, where the child never produced one).
pub(super) fn finish(
    emitter: &mut Emitter,
    source: &'static str,
    child_code: Option<i32>,
    error: RunnerError,
) -> RunnerError {
    emitter.emit(&Event::RunnerExit {
        code: i32::from(error.code()),
        source,
        child_code,
    });
    error
}

/// Emit the terminal [`Event::OutputCaptured`] for a run that had `--capture-dir`,
/// finalizing both streams' files and metadata first. A no-op without capture, so a
/// run that did not request it emits no such event (backward compatibility).
pub(super) fn emit_output_captured(emitter: &mut Emitter, capture: &Option<Capture>) {
    if let Some(capture) = capture {
        let (stdout, stderr) = capture.finalize();
        emitter.emit(&Event::OutputCaptured { stdout, stderr });
    }
}

/// The shared **hard** teardown tail — mark cleanup started, hard-kill the
/// container immediately (no soft stop), report the capture, and drop the
/// registry entry, in that order — for every decided ending that has no
/// soft-stop tier of its own: a clean natural exit, a wait failure (the
/// child's fate is unknown, so there is no outcome to soft-stop toward
/// either), and a control-plane `kill`. Routing all three through this one
/// site makes it structurally impossible for one of them to again drift from
/// the others, as the wait-failure branch once did (it used to return
/// through the bare [`finish`] instead, skipping this whole tail).
///
/// The three endings with a soft-stop tier (`timeout` / `cancel` /
/// `control_cancel`, in [`super::launch::run_async`]'s `Ending` match) are not funneled
/// through here: they run [`graceful_teardown`] between
/// `cleanup_started` and `cleanup_finished`, so their `cleanup_finished`
/// carries `Some(label)` instead of this function's fixed `None`. That is
/// the *only* difference in their tail — every other step matches this one.
pub(super) fn emit_hard_teardown(
    emitter: &mut Emitter,
    group: &ProcessGroup,
    capture: &Option<Capture>,
    registration: &Option<registry::Registration>,
) {
    emit_cleanup_started(emitter, group);
    emit_cleanup_finished(emitter, group, None);
    emit_output_captured(emitter, capture);
    clear_registration(registration);
}

/// Report a failed interactive terminal-foreground handoff to the JSONL stream and
/// end the run — the shared tail both terminal-handoff failure paths in
/// [`super::launch::run_async`] take (a failed [`super::launch::TerminalForegroundGuard::acquire`], and the
/// failed post-handoff [`ProcessGroup::resume`]).
///
/// **Why a new `phase`.** Both paths sit *after* the child has spawned (the
/// container exists and may hold live members) but *before* the `run_started` event
/// is written. Neither existing `phase` describes them: `create` is "the container
/// could not be created" and `attach` is "the launch into it failed" — here both
/// already succeeded, and it is the *interactive terminal handoff* that failed. So
/// this emits `container_failed` with the additive `phase: "foreground"`, an
/// additive value in the v1 schema's `phase` enum (no `schema_version` bump, per the
/// schema's versioning policy: adding an enum value only widens what a reader may
/// see, it does not change any existing shape). Emitting it here restores the stream
/// invariant that a terminal `runner_exit` with `source: "container_error"` is
/// always preceded by a describing `container_failed`, which these two paths
/// previously broke by leaving the reason on stderr alone.
///
/// **Order.** `container_failed` first, then the shared [`emit_hard_teardown`] tail
/// (the child was spawned, so the container must be torn down), then the terminal
/// [`finish`] `runner_exit` — mirroring the pre-spawn `container_failed` paths
/// ([`ProcessGroup::new`]/[`ProcessGroup::start`] failures), which likewise emit the
/// event before ending. Routing both paths through this one site keeps them from
/// drifting apart, exactly as [`emit_hard_teardown`] does for the hard-teardown
/// callers. `message` carries the underlying error verbatim (the sibling
/// `container_failed` paths use the same raw-error convention); the runner's own
/// contextual framing rides on `error` to stderr.
pub(super) fn finish_foreground_failure(
    emitter: &mut Emitter,
    group: &ProcessGroup,
    capture: &Option<Capture>,
    registration: &Option<registry::Registration>,
    error: RunnerError,
    message: String,
) -> RunnerError {
    emitter.emit(&Event::ContainerFailed {
        phase: "foreground",
        code: error.code(),
        message,
    });
    emit_hard_teardown(emitter, group, capture, registration);
    finish(emitter, "container_error", None, error)
}

/// Remove the registry entry on a decided ending. A no-op when registration was
/// skipped (best-effort) or already removed (idempotent).
pub(super) fn clear_registration(registration: &Option<registry::Registration>) {
    if let Some(registration) = registration {
        registration.remove();
    }
}

/// Snapshot the container's members — enriched with `ppid`/executable
/// `name`/`start_time` via [`ProcessGroup::members_info`] wherever the platform
/// can report them (`events::Member::from_info`) — and emit `members_snapshot`. A
/// read failure is a diagnostics gap, not a run failure, so it warns and skips the
/// event; it shares the same error contract as the bare-PID `members()` this
/// replaced (`ErrorReason::Io` only — a single vanished member is skipped, not an
/// error).
pub(super) fn emit_members_snapshot(emitter: &mut Emitter, group: &ProcessGroup) {
    match group.members_info() {
        Ok(infos) => emitter.emit(&Event::MembersSnapshot {
            members: infos.into_iter().map(Member::from_info).collect(),
        }),
        Err(err) => {
            eprintln!("processkit-cli: warning: could not snapshot container members: {err}");
        }
    }
}

/// The shared honest-degradation policy behind [`emit_cleanup_started`]'s
/// `members_before`/`read_error`: `Ok` reports the confirmed count, `Err` falls
/// back to the pre-existing `0` but flags it `read_error: true` instead of
/// letting it stand as a confirmed empty tree (see [`Event::CleanupStarted`]).
/// Pulled out as a pure function over a synthetic `Result` — like its
/// `emit_cleanup_finished`-side sibling [`remaining_pids_or_unknown`] — because
/// the real `Err` path is backend-internal plumbing not reliably forceable from a
/// spawned test child (see `hard_teardown_tail_emits_the_shared_sequence_in_order`'s
/// doc comment), so the honest-degradation branch is exercised here directly
/// instead of only through an always-`Ok` integration test (K-059: a real
/// injected failure, not a vacuous happy-path-only assertion).
fn members_len_or_unknown(members: Result<Vec<u32>, PkError>) -> (usize, bool) {
    match members {
        Ok(pids) => (pids.len(), false),
        Err(_) => (0, true),
    }
}

/// Mark the start of container teardown with the full tree size about to be
/// reaped. Emitted before any termination action (including the soft stop on a
/// runner-imposed ending), so `members_before` is the whole tree, not a post-soft
/// remnant.
///
/// A `group.members()` read failure is not silently fabricated as "0 members
/// before cleanup": it warns on stderr — the same honest-degradation convention
/// as the sibling [`emit_members_snapshot`] — and [`members_len_or_unknown`]'s
/// `read_error` flag tells a JSONL consumer the `0` fallback is not a confirmed
/// empty tree.
pub(super) fn emit_cleanup_started(emitter: &mut Emitter, group: &ProcessGroup) {
    let members = group.members();
    if let Err(err) = &members {
        eprintln!(
            "processkit-cli: warning: could not read container members before cleanup: {err}"
        );
    }
    let (members_before, read_error) = members_len_or_unknown(members);
    emitter.emit(&Event::CleanupStarted {
        members_before,
        read_error,
    });
}

/// The shared honest-degradation policy behind [`emit_cleanup_finished`]'s
/// `remaining`/`remaining_pids`/`read_error`: `Ok` reports the confirmed post-kill
/// snapshot, `Err` falls back to the pre-existing empty list but flags it
/// `read_error: true` instead of letting it stand as a confirmed-clean teardown
/// (see [`Event::CleanupFinished`], and [`members_len_or_unknown`] for why this is
/// a pure function unit-tested against a synthetic `Result`).
fn remaining_pids_or_unknown(members: Result<Vec<u32>, PkError>) -> (Vec<u32>, bool) {
    match members {
        Ok(pids) => (pids, false),
        Err(_) => (Vec::new(), true),
    }
}

/// Hard-kill the container and mark teardown finished with a post-kill member
/// snapshot. The hard kill is [`ProcessGroup::kill_all`] — the group's own kernel
/// teardown, the same mechanism its drop would run — invoked explicitly so
/// `remaining_pids` reflects the post-kill state rather than a pre-drop guess. Any
/// kill error is best-effort: the group's drop is still a backstop. `soft` labels
/// the soft-stop tier of a runner-imposed ending, or `None` on the natural-exit
/// path where no soft stop was attempted.
///
/// A post-kill `group.members()` read failure is not silently fabricated as "0
/// remaining, confirmed clean": it warns on stderr, matching the honest
/// degradation ProcessKit's own `ShutdownReport` applies to its member counts,
/// and [`remaining_pids_or_unknown`]'s `read_error` flag carries that same "not
/// confirmed" verdict onto the wire.
pub(super) fn emit_cleanup_finished(
    emitter: &mut Emitter,
    group: &ProcessGroup,
    teardown: Option<&GracefulTeardown>,
) {
    let _ = group.kill_all();
    let members = group.members();
    if let Err(err) = &members {
        eprintln!("processkit-cli: warning: could not read container members after cleanup: {err}");
    }
    let (remaining_pids, read_error) = remaining_pids_or_unknown(members);
    emitter.emit(&Event::CleanupFinished {
        remaining: remaining_pids.len(),
        remaining_pids,
        soft_terminate: teardown.map(|teardown| soft_terminate_label(teardown.soft)),
        shutdown: teardown.map(|teardown| teardown.shutdown.clone()),
        read_error,
    });
}

/// The machine label for a soft-stop tier, mirroring the honest stderr message.
pub(super) fn soft_terminate_label(soft: SoftTerminate) -> &'static str {
    match soft {
        SoftTerminate::Signalled => "signalled",
        SoftTerminate::Unsupported => "unsupported",
        SoftTerminate::Failed => "failed",
    }
}

/// A duration as whole milliseconds for the JSONL timing fields (`u64` is ample
/// for any deadline a run could carry; the source `Duration` is already bounded by
/// the CLI parser).
pub(super) fn duration_ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

/// The launch-failure event for a backend error, chosen by the runner-own code
/// rather than by re-matching the backend error: [`exit::SPAWN`] is a
/// `spawn_failed`, anything else a `container_failed` at the `attach` phase.
pub(super) fn launch_failure_event(err: &PkError, error: &RunnerError) -> Event {
    if error.code() == exit::SPAWN {
        Event::SpawnFailed {
            code: error.code(),
            message: err.to_string(),
        }
    } else {
        Event::ContainerFailed {
            phase: "attach",
            code: error.code(),
            message: err.to_string(),
        }
    }
}

/// The `runner_exit` `source` for a launch failure, paired with
/// [`launch_failure_event`].
pub(super) fn launch_failure_source(error: &RunnerError) -> &'static str {
    if error.code() == exit::SPAWN {
        "spawn_error"
    } else {
        "container_error"
    }
}

/// The shared reporting path for runner-imposed endings. ProcessKit owns the
/// actual soft stop, grace polling, and escalation; this layer reads the capability
/// first and projects the returned observations into the CLI's stable JSONL shape.
#[derive(Debug, Clone)]
pub(super) struct GracefulTeardown {
    soft: SoftTerminate,
    shutdown: ShutdownInfo,
}

/// Drive ProcessKit's reporting stop primitive. The capability scope is read
/// before the attempt, while the remaining fields come from its observed
/// `ShutdownReport`. If the driver itself errors, kill-on-drop/`kill_all` remains
/// the hard-stop backstop and the missing report facts stay explicitly null.
pub(super) async fn graceful_teardown(
    group: &ProcessGroup,
    grace: Option<Duration>,
) -> GracefulTeardown {
    let soft_stop_scope = group.soft_stop_scope().name();
    match group.stop(grace.unwrap_or_default(), true).await {
        Ok(report) => {
            let (soft, soft_signal) = match report.soft_signal() {
                SoftSignal::Sent(_) => (SoftTerminate::Signalled, "sent"),
                SoftSignal::Unsupported => (SoftTerminate::Unsupported, "unsupported"),
                SoftSignal::Failed(_) => (SoftTerminate::Failed, "failed"),
                _ => (SoftTerminate::Failed, "failed"),
            };
            GracefulTeardown {
                soft,
                shutdown: ShutdownInfo {
                    soft_stop_scope,
                    soft_signal,
                    members_before: report.members_before(),
                    members_after: report.members_after(),
                    drained_within_grace: Some(report.drained_within_grace()),
                    escalated: Some(report.escalated()),
                    elapsed_ms: Some(duration_ms(report.elapsed())),
                },
            }
        }
        Err(err) => {
            eprintln!("processkit-cli: warning: ProcessKit graceful stop failed: {err}");
            GracefulTeardown {
                soft: SoftTerminate::Failed,
                shutdown: ShutdownInfo {
                    soft_stop_scope,
                    soft_signal: "failed",
                    members_before: None,
                    members_after: None,
                    drained_within_grace: None,
                    escalated: None,
                    elapsed_ms: None,
                },
            }
        }
    }
}

/// Turn a runner-imposed ending into the reserved-band error it surfaces:
/// [`exit::TIMEOUT`] / [`exit::CANCELLED`] plus a message that names the ending
/// and describes, truthfully, how the tree was torn down.
pub(super) trait TeardownDescription {
    fn soft(&self) -> SoftTerminate;
    fn soft_stop_scope(&self) -> &'static str;
}

// Unit tests exercise isolated wording without a ProcessGroup. Keep their compact
// adapter out of production: real call sites always carry the observed
// `GracefulTeardown` and must never infer a capability scope from an outcome.
#[cfg(test)]
impl TeardownDescription for SoftTerminate {
    fn soft(&self) -> SoftTerminate {
        *self
    }

    fn soft_stop_scope(&self) -> &'static str {
        match self {
            SoftTerminate::Unsupported => "none",
            SoftTerminate::Signalled | SoftTerminate::Failed if cfg!(windows) => "opt_in_members",
            SoftTerminate::Signalled | SoftTerminate::Failed => "whole_tree",
        }
    }
}

impl TeardownDescription for &GracefulTeardown {
    fn soft(&self) -> SoftTerminate {
        self.soft
    }

    fn soft_stop_scope(&self) -> &'static str {
        self.shutdown.soft_stop_scope
    }
}

pub(super) fn termination_error<T: TeardownDescription>(
    kind: Termination,
    teardown: T,
    grace: Option<Duration>,
) -> RunnerError {
    let (code, headline) = match kind {
        // Both timeout triggers surface the same reserved code; only the headline
        // differs, naming which deadline elapsed so the stderr line is honest.
        Termination::Timeout {
            limit,
            trigger: TimeoutTrigger::Overall,
        } => (
            exit::TIMEOUT,
            format!("run timed out after {}", format_duration(limit)),
        ),
        Termination::Timeout {
            limit,
            trigger: TimeoutTrigger::Idle,
        } => (
            exit::TIMEOUT,
            format!(
                "run idle-timed out after {} with no output",
                format_duration(limit)
            ),
        ),
        // Every local-signal cancel surfaces the same reserved code; only the headline
        // differs, naming the signal that actually arrived (`Ctrl-C`, `SIGTERM`,
        // `SIGHUP`) so the stderr line is honest about who stopped the run.
        Termination::Cancelled(signal) => (
            exit::CANCELLED,
            format!("run cancelled ({})", signal.phrase()),
        ),
        Termination::ControlCancelled => (
            exit::CONTROL_CANCELLED,
            "run cancelled by a control-plane command".to_string(),
        ),
    };
    RunnerError::new(
        code,
        format!("{headline}: {}", describe_teardown(teardown, grace)),
    )
}

/// The error a control-plane `kill` surfaces: the reserved [`exit::CONTROL_KILLED`]
/// and a message stating, truthfully, that the whole tree was hard-killed at once —
/// no soft stop, no grace. Unlike [`termination_error`] there is no soft-terminate
/// tier or grace window to describe, because a kill has neither.
pub(super) fn control_kill_error() -> RunnerError {
    RunnerError::new(
        exit::CONTROL_KILLED,
        "run killed by a control-plane command: hard-killed the whole process tree \
         immediately via the container's kill-on-drop (no soft stop, no grace)"
            .to_string(),
    )
}

/// What a *delivered* soft stop actually was on this platform, as one honest
/// phrase for [`describe_teardown`]. The two platforms use genuinely different
/// mechanisms — a POSIX signal broadcast on Unix, a best-effort window close on a
/// Windows Job Object (which has no POSIX signal at all) — so the operator
/// message names the real one instead of a portable fiction.
///
/// Pure over an explicit `windows` flag rather than reading `cfg!` itself, so
/// **both** wordings are unit-tested on every host instead of leaving one branch
/// to whichever platform CI happens to run (K-059); the single call site passes
/// `cfg!(windows)`, which also keeps both arms compiled and type-checked
/// everywhere.
fn soft_stop_mechanism(windows: bool) -> &'static str {
    if windows {
        // ProcessKit 3's Windows soft tier: `WM_CLOSE` to every top-level window
        // owned by a live member plus a console `CTRL_BREAK` to an opted-in
        // leader. Deliberately not called a
        // signal — a Job Object has none — and deliberately free of the word
        // "grace", which belongs to the separate `--grace` wording this phrase is
        // spliced in front of.
        "asked the reachable process-tree members to close (WM_CLOSE for windowed members and \
         CTRL_BREAK for an opted-in console leader — a Job Object has no POSIX signal)"
    } else {
        "sent SIGTERM to the process tree"
    }
}

/// A truthful, human-readable description of the teardown that just happened —
/// the load-bearing part of the "honest degradation" contract. It states whether
/// a real soft stop was delivered (and by which mechanism, see
/// [`soft_stop_mechanism`]), whether a grace window was waited, and that the hard
/// kill is the container's kill-on-drop (a Windows Job Object terminate).
fn describe_teardown<T: TeardownDescription>(teardown: T, grace: Option<Duration>) -> String {
    let soft = teardown.soft();
    let scope = teardown.soft_stop_scope();
    let waited = match grace {
        Some(grace) => format!("waited {} grace, then ", format_duration(grace)),
        None => String::new(),
    };
    match soft {
        SoftTerminate::Signalled => format!(
            "{} (capability scope: {scope}), {waited}hard-killed any survivors via the container",
            soft_stop_mechanism(cfg!(windows)),
        ),
        // Windows-only in practice: every Unix backend always has a real SIGTERM
        // tier, so `Unsupported` can only come from a Job Object with nothing a
        // soft close could reach. The *reason* is what changed in ProcessKit 3 —
        // not "the platform has no soft tier" any more, but "this tree exposes
        // nothing that tier can trigger" — and the message says exactly that.
        SoftTerminate::Unsupported => format!(
            "the Windows pre-stop capability probe reported no soft-terminate target reachable (no \
             windowed member and no console-CTRL leader opted in), so — after {}— the tree \
             was hard-killed atomically via the Job Object",
            match grace {
                Some(grace) => format!("a {} grace delay ", format_duration(grace)),
                None => "no grace delay ".to_string(),
            }
        ),
        SoftTerminate::Failed => format!(
            "the soft-terminate request for capability scope {scope} could not be delivered, so \
             {waited}the process tree was hard-killed via the container"
        ),
    }
}

/// Map a `processkit` launch failure onto the runner-own exit-code band.
///
/// A locate/start failure is [`exit::SPAWN`] — the child never ran; every other
/// backend/containment failure is [`exit::BACKEND`]. A child's own exit is never
/// routed through here (it is an [`Outcome`], not an [`Err`]).
///
/// The failure mode is read off the borrowed [`PkErrorReason`] (ProcessKit 3
/// boxes it behind the pointer-sized [`PkError`] wrapper). The wildcard arm's
/// `{other}` renders exactly what `{err}` did before the migration:
/// [`PkError`]'s own `Display` delegates to the reason's, adding no envelope of
/// its own — so the operator-facing text is unchanged.
pub(super) fn map_launch_error(err: &PkError) -> RunnerError {
    match err.reason() {
        PkErrorReason::NotFound { .. } | PkErrorReason::Spawn { .. } => {
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
/// `Command::timeout` (the `--timeout` deadline is raced in [`super::launch::run_async`] and
/// reported as [`exit::TIMEOUT`] instead), so a `TimedOut` from the backend is an
/// invariant violation rather than a result.
pub(super) fn exit_code_for(outcome: Outcome) -> Result<i32, RunnerError> {
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
    use processkit::Command as PkCommand;

    use crate::capture::CAPTURE_MAX_BYTES;
    use crate::run::CancelSignal;

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
        // launch failure lands on the BACKEND code. `Io` is the one directly
        // constructible reason (a plain tuple variant), wrapped into the
        // pointer-sized `PkError` through its public `From<ErrorReason>`.
        let io = map_launch_error(&PkError::from(PkErrorReason::Io(std::io::Error::from(
            std::io::ErrorKind::AddrInUse,
        ))));
        assert_eq!(io.code(), exit::BACKEND);
    }

    #[test]
    fn timeout_and_cancel_carry_distinct_reserved_codes() {
        let timed_out = termination_error(
            Termination::Timeout {
                limit: Duration::from_secs(5),
                trigger: TimeoutTrigger::Overall,
            },
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        let cancelled = termination_error(
            Termination::Cancelled(CancelSignal::CtrlC),
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
            Termination::Timeout {
                limit: Duration::from_secs(5),
                trigger: TimeoutTrigger::Overall,
            },
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

    /// Both timeout triggers surface the reserved `TIMEOUT` code (an idle expiry is
    /// the *same class* of ending as an overall one — a deadline the runner enforced,
    /// per K-047/the task's exit-code decision), but their stderr headlines differ so
    /// an operator can tell "ran too long overall" from "went silent". `reason` on
    /// the JSONL `timeout` event is the machine-readable counterpart.
    #[test]
    fn idle_timeout_reuses_the_timeout_code_with_its_own_message() {
        let idle = termination_error(
            Termination::Timeout {
                limit: Duration::from_secs(3),
                trigger: TimeoutTrigger::Idle,
            },
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        let overall = termination_error(
            Termination::Timeout {
                limit: Duration::from_secs(3),
                trigger: TimeoutTrigger::Overall,
            },
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        // Same reserved class of ending, same code.
        assert_eq!(idle.code(), exit::TIMEOUT);
        assert_eq!(overall.code(), exit::TIMEOUT);

        let idle_msg = idle.to_string();
        assert!(
            idle_msg.contains("idle-timed out"),
            "an idle expiry names itself as an idle timeout: {idle_msg}"
        );
        assert!(
            idle_msg.contains("no output"),
            "the idle message states why (no output): {idle_msg}"
        );
        assert!(
            idle_msg.contains("3s"),
            "the idle window is echoed: {idle_msg}"
        );
        assert_ne!(
            idle_msg,
            overall.to_string(),
            "the idle and overall headlines must read differently"
        );

        // And the reason strings the JSONL event carries stay distinct.
        assert_eq!(TimeoutTrigger::Overall.reason(), "overall");
        assert_eq!(TimeoutTrigger::Idle.reason(), "idle");
    }

    #[test]
    fn cancel_message_names_ctrl_c() {
        let err = termination_error(
            Termination::Cancelled(CancelSignal::CtrlC),
            SoftTerminate::Signalled,
            None,
        );
        let msg = err.to_string();
        assert!(
            msg.contains("cancelled"),
            "message should say cancelled: {msg}"
        );
        assert!(msg.contains("Ctrl-C"), "message should name Ctrl-C: {msg}");
    }

    /// Every local stop signal is the *same class* of ending — a signal ended the run —
    /// so all of them keep the reserved `CANCELLED` code (K-047: an earlier, more
    /// specific record already disambiguates, here the `cancelled` event's `source`).
    /// What must **not** collapse is the reporting: the stderr headline names the
    /// signal that actually arrived, and the wire `source` values stay distinct, so a
    /// `systemctl stop` is never reported as a keyboard interrupt.
    #[cfg(unix)]
    #[test]
    fn unix_stop_signals_share_the_cancel_code_but_report_themselves_honestly() {
        let for_signal = |signal| {
            termination_error(
                Termination::Cancelled(signal),
                SoftTerminate::Signalled,
                Some(Duration::from_secs(2)),
            )
        };
        let ctrl_c = for_signal(CancelSignal::CtrlC);
        let sigterm = for_signal(CancelSignal::Term);
        let sighup = for_signal(CancelSignal::Hup);

        // One class of ending, one reserved code.
        for err in [&ctrl_c, &sigterm, &sighup] {
            assert_eq!(err.code(), exit::CANCELLED);
        }

        // Distinct, honest headlines.
        let sigterm_msg = sigterm.to_string();
        assert!(
            sigterm_msg.contains("run cancelled (SIGTERM)"),
            "a SIGTERM cancel must name SIGTERM: {sigterm_msg}"
        );
        assert!(
            !sigterm_msg.contains("Ctrl-C"),
            "a SIGTERM is not a Ctrl-C: {sigterm_msg}"
        );
        let sighup_msg = sighup.to_string();
        assert!(
            sighup_msg.contains("run cancelled (SIGHUP)"),
            "a SIGHUP cancel must name SIGHUP: {sighup_msg}"
        );
        assert_ne!(ctrl_c.to_string(), sigterm_msg);
        assert_ne!(sigterm_msg, sighup_msg);

        // And the machine-readable `source` values a consumer switches on.
        assert_eq!(CancelSignal::CtrlC.source(), "ctrl_c");
        assert_eq!(CancelSignal::Term.source(), "sigterm");
        assert_eq!(CancelSignal::Hup.source(), "sighup");
    }

    /// The Windows sibling of the Unix proof above: every console-control event
    /// shares the reserved `CANCELLED` code (the same class of ending) but keeps a
    /// distinct, honest `source`/stderr headline — a console close is never
    /// reported as a keyboard interrupt, a logoff, or a shutdown.
    #[cfg(windows)]
    #[test]
    fn windows_ctrl_events_share_the_cancel_code_but_report_themselves_honestly() {
        let for_signal = |signal| {
            termination_error(
                Termination::Cancelled(signal),
                SoftTerminate::Unsupported,
                Some(Duration::from_secs(2)),
            )
        };
        let ctrl_c = for_signal(CancelSignal::CtrlC);
        let ctrl_break = for_signal(CancelSignal::CtrlBreak);
        let ctrl_close = for_signal(CancelSignal::CtrlClose);
        let ctrl_logoff = for_signal(CancelSignal::CtrlLogoff);
        let ctrl_shutdown = for_signal(CancelSignal::CtrlShutdown);

        // One class of ending, one reserved code.
        for err in [
            &ctrl_c,
            &ctrl_break,
            &ctrl_close,
            &ctrl_logoff,
            &ctrl_shutdown,
        ] {
            assert_eq!(err.code(), exit::CANCELLED);
        }

        // Distinct, honest headlines.
        let ctrl_break_msg = ctrl_break.to_string();
        let ctrl_close_msg = ctrl_close.to_string();
        let ctrl_logoff_msg = ctrl_logoff.to_string();
        let ctrl_shutdown_msg = ctrl_shutdown.to_string();
        assert!(
            ctrl_break_msg.contains("run cancelled (Ctrl-Break)"),
            "the message must name Ctrl-Break: {ctrl_break_msg}"
        );
        assert!(
            ctrl_close_msg.contains("run cancelled (console close)"),
            "the message must name console close: {ctrl_close_msg}"
        );
        assert!(
            ctrl_logoff_msg.contains("run cancelled (logoff)"),
            "the message must name logoff: {ctrl_logoff_msg}"
        );
        assert!(
            ctrl_shutdown_msg.contains("run cancelled (system shutdown)"),
            "the message must name system shutdown: {ctrl_shutdown_msg}"
        );
        let messages = [
            ctrl_c.to_string(),
            ctrl_break_msg,
            ctrl_close_msg,
            ctrl_logoff_msg,
            ctrl_shutdown_msg,
        ];
        for (i, a) in messages.iter().enumerate() {
            for b in &messages[i + 1..] {
                assert_ne!(a, b, "two distinct triggers produced the same message");
            }
        }

        // And the machine-readable `source`/human `phrase` values.
        assert_eq!(CancelSignal::CtrlBreak.source(), "ctrl_break");
        assert_eq!(CancelSignal::CtrlBreak.phrase(), "Ctrl-Break");
        assert_eq!(CancelSignal::CtrlClose.source(), "ctrl_close");
        assert_eq!(CancelSignal::CtrlClose.phrase(), "console close");
        assert_eq!(CancelSignal::CtrlLogoff.source(), "ctrl_logoff");
        assert_eq!(CancelSignal::CtrlLogoff.phrase(), "logoff");
        assert_eq!(CancelSignal::CtrlShutdown.source(), "ctrl_shutdown");
        assert_eq!(CancelSignal::CtrlShutdown.phrase(), "system shutdown");
    }

    #[test]
    fn the_four_runner_imposed_endings_carry_distinct_codes() {
        // Every runner-imposed ending must be tellable apart by exit code: a timeout,
        // a Ctrl-C, a control-plane cancel, and a control-plane kill.
        let timeout = termination_error(
            Termination::Timeout {
                limit: Duration::from_secs(5),
                trigger: TimeoutTrigger::Overall,
            },
            SoftTerminate::Signalled,
            None,
        );
        let ctrl_c = termination_error(
            Termination::Cancelled(CancelSignal::CtrlC),
            SoftTerminate::Signalled,
            None,
        );
        let control_cancel = termination_error(
            Termination::ControlCancelled,
            SoftTerminate::Signalled,
            None,
        );
        let control_kill = control_kill_error();
        let codes = [
            timeout.code(),
            ctrl_c.code(),
            control_cancel.code(),
            control_kill.code(),
        ];
        assert_eq!(control_cancel.code(), exit::CONTROL_CANCELLED);
        assert_eq!(control_kill.code(), exit::CONTROL_KILLED);
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b, "two runner-imposed endings collided on code {a}");
            }
        }
    }

    #[test]
    fn control_cancel_message_names_the_command_and_describes_teardown() {
        // A control-plane cancel shares the honest teardown wording (it is the same
        // path as Ctrl-C) but names the *command* as the trigger, not the keyboard.
        let err = termination_error(
            Termination::ControlCancelled,
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        let msg = err.to_string();
        assert!(
            msg.contains("control-plane command"),
            "message should name the control command: {msg}"
        );
        assert!(
            !msg.contains("Ctrl-C"),
            "a control cancel is not a Ctrl-C: {msg}"
        );
        assert!(
            msg.contains(soft_stop_mechanism(cfg!(windows))),
            "the shared teardown is described: {msg}"
        );
        assert!(msg.contains("2s"), "the grace is echoed: {msg}");
    }

    #[test]
    fn control_kill_message_is_immediate_and_ungraceful() {
        let err = control_kill_error();
        let msg = err.to_string();
        assert!(msg.contains("killed"), "message should say killed: {msg}");
        assert!(msg.contains("immediately"), "a kill is immediate: {msg}");
        assert!(
            msg.contains("no soft stop") && msg.contains("no grace"),
            "a kill waits for nothing: {msg}"
        );
        assert!(msg.contains("hard-killed"), "the hard kill is named: {msg}");
    }

    /// Both platform wordings of a *delivered* soft stop, exercised on every host
    /// (the point of [`soft_stop_mechanism`] taking the flag rather than reading
    /// `cfg!`): the Unix phrase names the real POSIX signal, and the Windows one
    /// names what a Job Object can actually do — never a signal it has no way to
    /// send.
    #[test]
    fn a_delivered_soft_stop_names_the_mechanism_the_platform_really_used() {
        let unix = soft_stop_mechanism(false);
        assert!(unix.contains("SIGTERM"), "{unix}");
        assert!(!unix.contains("WM_CLOSE"), "{unix}");

        let windows = soft_stop_mechanism(true);
        assert!(
            windows.contains("WM_CLOSE"),
            "the Windows soft tier is a window close: {windows}"
        );
        assert!(
            !windows.contains("SIGTERM"),
            "a Job Object has no POSIX signal, so the message must not claim one: {windows}"
        );
        assert_ne!(
            unix, windows,
            "the two platforms do genuinely different things and must not share wording"
        );
    }

    #[test]
    fn teardown_reports_a_real_soft_stop_and_the_grace() {
        // Where a soft stop was delivered, the message names this platform's real
        // mechanism and states the grace was waited.
        let msg = describe_teardown(SoftTerminate::Signalled, Some(Duration::from_secs(2)));
        assert!(msg.contains(soft_stop_mechanism(cfg!(windows))), "{msg}");
        assert!(msg.contains("2s"), "{msg}");
        assert!(msg.contains("grace"), "{msg}");
        assert!(msg.contains("hard-killed"), "{msg}");
        assert!(
            !msg.contains("could not be delivered"),
            "a delivered soft stop is not a failure: {msg}"
        );
    }

    #[test]
    fn unsupported_teardown_is_reported_honestly_without_pretending() {
        // The "honest degradation" contract: when no soft stop could be delivered,
        // the message says so plainly and names the atomic Job Object kill — it
        // must never imply a graceful soft-terminate was performed. Since
        // ProcessKit 3 the *reason* is that this particular tree exposes nothing a
        // Windows soft close can reach (no window, no console-CTRL leader), not
        // that the platform has no soft tier at all — so the message must state
        // the tree-specific reason rather than the retired blanket claim.
        let msg = describe_teardown(SoftTerminate::Unsupported, Some(Duration::from_secs(2)));
        assert!(msg.contains("Windows"), "{msg}");
        assert!(msg.contains("Job Object"), "{msg}");
        assert!(msg.contains("no soft-terminate"), "{msg}");
        assert!(
            msg.contains("no windowed member") && msg.contains("no console-CTRL leader"),
            "the honest reason is that nothing in this tree could receive a soft close: {msg}"
        );
        assert!(
            !msg.contains("has no soft-terminate signal yet"),
            "ProcessKit 3 does have a Windows soft tier; the retired blanket claim \
             must not survive: {msg}"
        );
        assert!(
            !msg.contains("sent SIGTERM"),
            "must not claim a soft signal was delivered: {msg}"
        );
        assert!(
            !msg.contains("WM_CLOSE"),
            "nothing was closed either — this is the reached-nothing branch: {msg}"
        );
    }

    #[test]
    fn teardown_without_grace_omits_the_grace_wording() {
        let msg = describe_teardown(SoftTerminate::Signalled, None);
        assert!(msg.contains(soft_stop_mechanism(cfg!(windows))), "{msg}");
        assert!(!msg.contains("grace"), "no grace was configured: {msg}");
    }

    #[test]
    fn failed_soft_terminate_is_reported_but_still_hard_kills() {
        let msg = describe_teardown(SoftTerminate::Failed, Some(Duration::from_secs(1)));
        assert!(msg.contains("could not be delivered"), "{msg}");
        assert!(msg.contains("hard-killed"), "{msg}");
    }

    /// Forcing a real wait *failure* through the child's actual OS-level wait
    /// call is practically unreachable from a test (`RunningProcess::wait`'s own
    /// `Err` path is backend-internal plumbing, not something a spawned test
    /// child can be made to trigger deterministically) — and the same is true
    /// of forcing `exit_code_for` into its own `Err` arm (an untimed
    /// `Outcome::TimedOut` or an unrecognized `#[non_exhaustive]` variant is not
    /// producible by this crate's real backend from the test arsenal either).
    /// So this proves the thing that *is* reachable and is the actual fix:
    /// [`emit_hard_teardown`], the exact shared tail both of those `Ending::Exited`
    /// error arms now run (see the `Err(err)` arm on the wait itself, and the
    /// `Err(error)` arm on `exit_code_for(outcome)`, in `run_async`), fires
    /// `cleanup_started` → the hard kill via `cleanup_finished` (with no
    /// soft-terminate tier) → `output_captured` → nothing else, in that order,
    /// for *any* caller — natural exit, control-kill, and both decode-failure
    /// paths alike. A future edit that special-cases one of those callers back
    /// out of this shared function (as the wait-failure path used to be) has
    /// nowhere to silently diverge: it would have to stop calling this helper,
    /// which is visible on review.
    #[tokio::test]
    async fn hard_teardown_tail_emits_the_shared_sequence_in_order() {
        let group = ProcessGroup::new().expect("create a ProcessGroup");
        let command = if cfg!(windows) {
            PkCommand::new("cmd").args(["/c", "exit", "0"])
        } else {
            PkCommand::new("true")
        };
        let running = group
            .start(&command)
            .await
            .expect("start a trivial, fast-exiting child");
        running.wait().await.expect("the trivial child exits");

        let dir = std::env::temp_dir().join(format!(
            "processkit-cli-run-unit-hard-teardown-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after the epoch")
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let jsonl = dir.join("events.jsonl");
        let mut emitter = Emitter::create(&jsonl).expect("create the events file");
        // A real `Capture` (not `None`) so `output_captured` actually fires too —
        // proving all three events, not just the two cleanup ones.
        let capture = Some(
            Capture::create(&dir.join("capture"), CAPTURE_MAX_BYTES)
                .expect("create the capture dir"),
        );

        emit_hard_teardown(&mut emitter, &group, &capture, &None);

        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&jsonl)
            .expect("read the events file back")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
            .collect();
        let kinds: Vec<&str> = lines
            .iter()
            .map(|value| value["event"].as_str().expect("every event has a tag"))
            .collect();
        assert_eq!(
            kinds,
            vec!["cleanup_started", "cleanup_finished", "output_captured"],
            "the shared hard-teardown tail must emit exactly these three events \
             in this order for every caller"
        );
        assert!(
            lines[1]["soft_terminate"].is_null(),
            "the hard-teardown tail never soft-stops, so cleanup_finished's \
             soft_terminate must be null: {:?}",
            lines[1]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both interactive terminal-handoff failure paths — a failed
    /// `TerminalForegroundGuard::acquire` and the failed post-handoff
    /// `group.resume()`, both in `run_async` — now route through
    /// [`finish_foreground_failure`], which restores the stream invariant that a
    /// terminal `runner_exit` with `source: "container_error"` is always preceded by
    /// a describing `container_failed` (previously these two paths emitted only the
    /// teardown pair and the terminal exit, leaving the reason on stderr alone).
    ///
    /// Driving `run_async` itself into those branches needs a real controlling
    /// terminal plus a `tcsetpgrp`/`resume` that fails on demand — not reachable
    /// deterministically from a test. So, like the sibling
    /// `hard_teardown_tail_emits_the_shared_sequence_in_order`, this exercises the
    /// exact shared site both paths take, with a real `ProcessGroup`/`Emitter`
    /// (K-015, no mocks), and pins the emitted sequence and the ordering invariant.
    #[tokio::test]
    async fn foreground_failure_emits_container_failed_before_the_terminal_exit() {
        let group = ProcessGroup::new().expect("create a ProcessGroup");
        let command = if cfg!(windows) {
            PkCommand::new("cmd").args(["/c", "exit", "0"])
        } else {
            PkCommand::new("true")
        };
        let running = group
            .start(&command)
            .await
            .expect("start a trivial, fast-exiting child");
        running.wait().await.expect("the trivial child exits");

        let dir = std::env::temp_dir().join(format!(
            "processkit-cli-run-unit-foreground-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after the epoch")
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let jsonl = dir.join("events.jsonl");
        let mut emitter = Emitter::create(&jsonl).expect("create the events file");

        // The `RunnerError` carries the runner's contextual framing (→ stderr); the
        // separate `message` is the underlying error the `container_failed` records.
        let error = RunnerError::new(
            exit::BACKEND,
            "could not give the interactive child terminal control: simulated".to_string(),
        );
        let returned = finish_foreground_failure(
            &mut emitter,
            &group,
            &None,
            &None,
            error,
            "simulated terminal-handoff failure".to_string(),
        );
        // Like `finish`, the error is returned unchanged (the reserved BACKEND code).
        assert_eq!(returned.code(), exit::BACKEND);

        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&jsonl)
            .expect("read the events file back")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
            .collect();
        let kinds: Vec<&str> = lines
            .iter()
            .map(|value| value["event"].as_str().expect("every event has a tag"))
            .collect();
        assert_eq!(
            kinds,
            vec![
                "container_failed",
                "cleanup_started",
                "cleanup_finished",
                "runner_exit",
            ],
            "a terminal-handoff failure emits the describing container_failed first, \
             then the hard-teardown pair, then the terminal runner_exit"
        );

        // The describing event carries the new `foreground` phase, the BACKEND code,
        // and the raw underlying message.
        assert_eq!(lines[0]["phase"], "foreground");
        assert_eq!(lines[0]["code"], exit::BACKEND);
        assert_eq!(lines[0]["message"], "simulated terminal-handoff failure");

        // The invariant this task restores: the terminal `container_error`
        // `runner_exit` is preceded by a `container_failed` — it is no longer the
        // lone record of the failure.
        let runner_exit = lines.last().expect("a terminal event");
        assert_eq!(runner_exit["event"], "runner_exit");
        assert_eq!(runner_exit["source"], "container_error");
        assert_eq!(runner_exit["code"], i32::from(exit::BACKEND));
        assert!(
            runner_exit["child_code"].is_null(),
            "a runner-own failure forwards no child code: {runner_exit}"
        );
        let container_failed_at = kinds
            .iter()
            .position(|k| *k == "container_failed")
            .expect("container_failed present");
        let runner_exit_at = kinds
            .iter()
            .position(|k| *k == "runner_exit")
            .expect("runner_exit present");
        assert!(
            container_failed_at < runner_exit_at,
            "container_failed must precede the terminal runner_exit: {kinds:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `emit_cleanup_started`'s real `Err` path (a `group.members()` read
    /// failure) is not reliably forceable from a spawned test child — like the
    /// sibling `Err` arms this module already documents as untestable through the
    /// real backend (see `hard_teardown_tail_emits_the_shared_sequence_in_order`'s
    /// doc comment) — so the honest-degradation policy is proven directly against
    /// the pure [`members_len_or_unknown`] with a real injected `Err`, not just an
    /// always-`Ok` happy path (K-059).
    #[test]
    fn members_len_or_unknown_flags_a_read_failure_instead_of_a_confirmed_zero() {
        assert_eq!(
            members_len_or_unknown(Ok(vec![4242, 4243])),
            (2, false),
            "a successful read reports the real count, unflagged"
        );
        assert_eq!(
            members_len_or_unknown(Ok(Vec::new())),
            (0, false),
            "a successful read of an empty tree is a confirmed zero, unflagged — \
             distinct from the failure fallback below despite the same count"
        );
        let simulated = PkError::from(PkErrorReason::Io(std::io::Error::other(
            "simulated members() failure",
        )));
        assert_eq!(
            members_len_or_unknown(Err(simulated)),
            (0, true),
            "a read failure must not be indistinguishable from a confirmed-empty tree"
        );
    }

    /// The `emit_cleanup_finished` twin of the test above, over the pure
    /// [`remaining_pids_or_unknown`] — same K-059 rationale (the real `Err` path
    /// is backend-internal plumbing, not deterministically triggerable from a
    /// spawned test child).
    #[test]
    fn remaining_pids_or_unknown_flags_a_read_failure_instead_of_a_confirmed_clean_teardown() {
        assert_eq!(
            remaining_pids_or_unknown(Ok(vec![4242])),
            (vec![4242], false),
            "a successful read reports the real snapshot, unflagged"
        );
        assert_eq!(
            remaining_pids_or_unknown(Ok(Vec::new())),
            (Vec::new(), false),
            "a successful read of an empty tree is a confirmed-clean teardown, \
             unflagged — distinct from the failure fallback below despite the \
             same empty snapshot"
        );
        let simulated = PkError::from(PkErrorReason::Io(std::io::Error::other(
            "simulated members() failure",
        )));
        assert_eq!(
            remaining_pids_or_unknown(Err(simulated)),
            (Vec::new(), true),
            "a read failure must not be indistinguishable from a confirmed-clean teardown"
        );
    }
}
