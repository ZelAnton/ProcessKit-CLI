//! `wait`: block until a run recorded in the per-user registry has finished.
//!
//! A supervisor that *started* a run can simply wait on the runner as a child
//! process. One that did not — an adapter that restarted, a dashboard, a cleanup
//! step, anything holding only a `run_id` — has no such handle, and until now had
//! only one option: hand-roll a polling loop around `inspect` and reverse-engineer
//! run lifetime from [`exit::CONTROL`] refusals. That is a bad contract to ask of a
//! caller (a `CONTROL` failure means "I could not reach it", which is *not* the same
//! as "it finished"), so this module makes waiting a first-class, read-only command:
//!
//! ```text
//! processkit-cli wait --run-id build-42 [--timeout 10m]
//! ```
//!
//! It is the *lifetime* counterpart to [`crate::list`]'s discovery and
//! [`crate::prune`]'s cleanup, and like both it opens the registry through
//! [`registry::Registry::open_read_only`] — never the mutating
//! [`registry::Registry::open`] `run` uses — so waiting on a run cannot itself create
//! the registry directory or touch its permissions. Unlike `inspect`/`cancel`/`kill`
//! it never connects to the run's control transport and never asks the runner for
//! anything: the run is not disturbed, not ended, and not even aware it is being
//! waited on. It follows that a run whose control endpoint never came up (best-effort
//! transport degradation, see [`registry::Record::endpoint`]) is still perfectly
//! waitable — `wait` needs no endpoint.
//!
//! # Polling, and why
//!
//! Liveness lives in an **OS advisory lock** the runner holds for the whole run
//! (`docs/registry.md`, "Staleness"). That lock is exactly what makes an abruptly
//! killed runner detectable — the OS releases it on death by any means — but it comes
//! with no event, notification, or wakeup a third process could subscribe to, and the
//! registry publishes no such channel either. Waiting on it is therefore *periodic
//! probing*, at [`POLL_INTERVAL`], and this module says so rather than pretending to
//! be event-driven. The cost is one directory scan plus one non-blocking lock attempt
//! per matching record per tick; the latency cost is bounded by the same interval.
//!
//! Blocking on the lock itself (`flock` without `LOCK_NB`) would look event-driven and
//! be wrong: acquiring a stale entry's lock is how a *reclaimer* claims an entry (see
//! [`registry::Registry::prune`]), so a waiter that blocked for it would end up
//! holding — and briefly denying — the very lock the registry's staleness contract is
//! built on, and would still learn nothing about a record that disappears without its
//! lock ever changing hands (the clean-exit path deletes both files).
//!
//! # The three outcomes
//!
//! | Exit | Meaning |
//! | --- | --- |
//! | `0` | The run is over: its record is gone, or every record under that id probed as stale. |
//! | [`exit::WAIT_TIMEOUT`] (112) | `--timeout` elapsed while the run was still live. **The waiter** gave up; the run was untouched and is still going. |
//! | [`exit::CONTROL`] (103) | More than one live run is registered under that `run_id`, so there is no single run to wait for. |
//!
//! Nothing is printed on success: the exit code *is* the answer, so `wait` has no
//! output format to keep stable (and no `--json` to add one). A failure explains
//! itself on stderr, like every other subcommand.
//!
//! # An unknown `run_id` reads as "finished"
//!
//! This is the module's one genuinely arbitrary-looking decision, and it is
//! deliberate. A run that exits cleanly **deletes its own registry entry**
//! ([`registry::Registration::remove`]), and the registry keeps no history of what
//! used to be there. From the outside, "`build-42` was never registered" and
//! "`build-42` finished a second before you asked" are the *same observation*: no
//! matching record. No amount of scanning can separate them.
//!
//! So both are answered the same way — exit `0`, "it is not running" — rather than
//! inventing a third outcome that could only ever be a guess. The alternative,
//! failing on an unknown id, would be worse than merely unhelpful: it would make the
//! result depend on *when* the caller asked, turning the ordinary and expected race
//! (the run finished while the adapter was starting up) into a hard error, which is
//! precisely the failure mode a `wait` command exists to remove.
//!
//! The consequence a caller must plan for is the mirror image: **a typo in a `run_id`
//! returns `0` immediately**, indistinguishable from a fast, successful run. A caller
//! that needs to know a run really existed must establish that separately — it
//! launched the run itself, or it saw the id in [`crate::list`] — and must not read
//! `wait`'s `0` as proof of existence. See `docs/registry.md`, "Waiting — `wait`".

use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::control;
use crate::duration_fmt::format_duration;
use crate::exit::{self, RunnerError};
use crate::registry::{self, RunStatus};

/// How long to sleep between registry probes.
///
/// A compromise, not a tuned constant: short enough that a caller chaining work onto
/// a finished run does not feel it (a quarter second on top of a run measured in
/// seconds or minutes), long enough that a multi-hour wait costs a negligible number
/// of directory scans. It bounds only the *detection* latency — the moment the run
/// actually ends is unaffected, since `wait` never touches the run.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The `--timeout` a caller asked for, paired with the instant it expires — kept
/// together so the give-up path can report the requested duration without an
/// `Option` it would have to unwrap, and so the deadline is computed exactly once
/// (never re-derived from a drifting "now") for the whole wait.
struct WaitDeadline {
    /// The `--timeout` value as the caller wrote it, for the diagnostic.
    limit: Duration,
    /// When the wait gives up.
    at: Instant,
}

impl WaitDeadline {
    /// A deadline `limit` from now.
    fn new(limit: Duration) -> Self {
        Self {
            limit,
            at: Instant::now() + limit,
        }
    }

    /// How long to sleep before the next probe: a full [`POLL_INTERVAL`], or the
    /// shorter remainder when the deadline is nearer than that (so a
    /// `--timeout 100ms` is honored at ~100ms rather than rounded up to the poll
    /// step). `None` once the deadline has been reached — the caller has already
    /// probed by then, so no chance to observe the run's end is lost to rounding.
    fn next_step(&self) -> Option<Duration> {
        let remaining = self.at.checked_duration_since(Instant::now())?;
        if remaining.is_zero() {
            return None;
        }
        Some(remaining.min(POLL_INTERVAL))
    }
}

/// Run `wait --run-id <id> [--timeout <duration>]`: poll the per-user registry until
/// the named run is no longer live, then exit `0`.
///
/// The loop probes **first** and sleeps second, so a run that is already over costs a
/// single scan and returns immediately, and the last probe of a bounded wait happens
/// *at* the deadline rather than one poll step before it.
///
/// Each pass asks [`registry::Registry::probe_run`] one question and acts on its
/// four-way answer:
///
/// - [`RunStatus::Finished`] — confirmed over (no record, or every matching record
///   probed stale): success.
/// - [`RunStatus::Live`] — exactly one live record: keep waiting, which is the whole
///   job.
/// - [`RunStatus::Unprobed`] — nothing confirmed live, but a matching record's
///   liveness could not be read at all: **also keep waiting**. Reporting success here
///   would claim a run had finished on the strength of a probe that never ran; a
///   bounded caller still gets a definite answer when `--timeout` elapses
///   ([`exit::WAIT_TIMEOUT`], honestly meaning "could not confirm completion in
///   time"), and an unbounded one keeps waiting for a verdict rather than
///   manufacturing one. This is the same "unknown is not confirmed" stance
///   [`registry::Registry::prune`] takes when it refuses to reap an unprobeable entry.
/// - [`RunStatus::Ambiguous`] — several live runs share the id, so there is no single
///   run whose end could be waited for: fail immediately with the same
///   [`exit::CONTROL`] verdict, and the same wording, every other by-`run-id` client
///   gives an ambiguous id ([`control::ambiguous_run`]). Re-checked on every pass, not
///   just the first, since a duplicate can register at any moment; waiting it out
///   would mean silently tracking whichever entry the scan happened to yield first.
///
/// A registry that cannot be opened or read at all is an [`exit::SETUP`] failure, as
/// it is for `list`/`prune`: a support/prerequisite problem the caller can usually act
/// on (a bad `PROCESSKIT_CLI_REGISTRY_DIR`, denied permissions), and emphatically not
/// an answer about the run.
pub fn run(run_id: &str, timeout: Option<Duration>) -> Result<(), RunnerError> {
    let registry = registry::Registry::open_read_only().map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not open the run registry: {err}"),
        )
    })?;
    let deadline = timeout.map(WaitDeadline::new);

    loop {
        let status = registry.probe_run(run_id).map_err(|err| {
            RunnerError::new(
                exit::SETUP,
                format!("could not read the run registry: {err}"),
            )
        })?;
        match status {
            RunStatus::Finished => return Ok(()),
            RunStatus::Ambiguous { live } => {
                return Err(control::ambiguous_run("wait for", run_id, live));
            }
            // Still going, or not confirmed over — either way, keep waiting.
            RunStatus::Live | RunStatus::Unprobed => {}
        }

        match &deadline {
            None => sleep(POLL_INTERVAL),
            Some(deadline) => match deadline.next_step() {
                Some(step) => sleep(step),
                None => return Err(wait_timed_out(run_id, deadline.limit, status)),
            },
        }
    }
}

/// The give-up error: `--timeout` elapsed without the run being confirmed finished.
///
/// Worded to keep the two deadlines apart in the reader's head — it names *waiting* as
/// what stopped, and never suggests the run was ended — because the one dangerous
/// misreading of [`exit::WAIT_TIMEOUT`] is "the run timed out", which is what
/// [`exit::TIMEOUT`] (106) means and this never does.
///
/// `last` is the status the final probe returned, and the message reports exactly that
/// rather than a convenient summary: the two ways a bounded wait can run out are
/// genuinely different facts, and claiming a confidently "still live" run when the
/// truth was "its entry could not be probed" would be the same kind of unearned
/// certainty this command refuses everywhere else.
fn wait_timed_out(run_id: &str, limit: Duration, last: RunStatus) -> RunnerError {
    let observed = match last {
        RunStatus::Live => "it is still live and was left running",
        RunStatus::Unprobed => {
            "it was not confirmed finished — a matching registry entry could not be \
             probed, and an unprobeable entry is never read as a completed run"
        }
        // Neither of these can be the *last* status seen: both end the wait
        // immediately, above. Spelled out rather than caught by a wildcard so this
        // match stays total, and worded conservatively in case that ever changes.
        RunStatus::Finished | RunStatus::Ambiguous { .. } => "it was not confirmed finished",
    };
    RunnerError::new(
        exit::WAIT_TIMEOUT,
        format!(
            "stopped waiting for run `{run_id}` after {}: {observed} — \
             raise or drop `--timeout` to keep waiting",
            format_duration(limit)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The give-up error carries the reserved `WAIT_TIMEOUT` code — never the run's
    /// own `TIMEOUT` — and says, in as many words, that the run survived the wait.
    #[test]
    fn wait_timed_out_uses_the_waiters_own_code_and_says_the_run_survived() {
        let err = wait_timed_out("build-42", Duration::from_secs(5), RunStatus::Live);
        assert_eq!(err.code(), exit::WAIT_TIMEOUT);
        assert_ne!(
            err.code(),
            exit::TIMEOUT,
            "a waiter's deadline must never be reported as the run's deadline"
        );
        let message = err.to_string();
        assert!(message.contains("build-42"), "names the run: {message}");
        assert!(
            message.contains("still live"),
            "states the run outlived the wait: {message}"
        );
        assert!(
            message.contains("5s"),
            "echoes the requested deadline: {message}"
        );
    }

    /// The deadline renders through the shared [`format_duration`] — the same
    /// compact, honest form `run` uses — not `Duration`'s `{:?}` Debug output,
    /// which would print `1.5s` for a `1500ms` timeout and disagree with `run`'s
    /// own diagnostics for the identical value.
    #[test]
    fn wait_timed_out_uses_the_shared_compact_duration_rendering() {
        let err = wait_timed_out("build-42", Duration::from_millis(1500), RunStatus::Live);
        let message = err.to_string();
        assert!(
            message.contains("1500ms"),
            "renders the compact form, matching `run`'s diagnostics: {message}"
        );
        assert!(
            !message.contains("1.5s"),
            "must not fall back to `Duration`'s Debug rendering: {message}"
        );
    }

    /// Giving up on an *unprobeable* entry reports that, not a confident "still live"
    /// the last probe never established — the same refusal to overstate an unconfirmed
    /// observation that keeps `wait` from calling such an entry finished.
    #[test]
    fn wait_timed_out_does_not_claim_liveness_it_never_confirmed() {
        let err = wait_timed_out("build-42", Duration::from_secs(5), RunStatus::Unprobed);
        assert_eq!(err.code(), exit::WAIT_TIMEOUT);
        let message = err.to_string();
        assert!(
            message.contains("not confirmed finished") && message.contains("could not be probed"),
            "names the unprobeable entry as the reason: {message}"
        );
        assert!(
            !message.contains("still live"),
            "must not assert liveness the probe never confirmed: {message}"
        );
    }

    /// An ambiguous id is the same `CONTROL` verdict — and the same wording — every
    /// other by-`run-id` client gives, phrased for this command's own action.
    #[test]
    fn ambiguity_reuses_the_shared_control_verdict() {
        let err = control::ambiguous_run("wait for", "dup-id", 2);
        assert_eq!(err.code(), exit::CONTROL);
        let message = err.to_string();
        assert!(
            message.contains("cannot wait for run `dup-id`"),
            "reads as a sentence about waiting: {message}"
        );
        assert!(message.contains("ambiguous"), "names the reason: {message}");
    }

    /// The sleep between probes is bounded twice over: by the poll step, and by the
    /// caller's own deadline — so a bounded wait never naps past its `--timeout`.
    #[test]
    fn next_step_is_capped_by_the_poll_interval_and_by_the_deadline() {
        // A distant deadline: only the poll cap binds, so the step is exactly one
        // interval.
        let far = WaitDeadline::new(Duration::from_secs(3600));
        assert_eq!(
            far.next_step(),
            Some(POLL_INTERVAL),
            "a distant deadline sleeps a full poll step"
        );

        // A deadline nearer than one poll step: the step shrinks to fit inside it.
        let near_limit = Duration::from_millis(200);
        assert!(
            near_limit < POLL_INTERVAL,
            "this case is only meaningful while the fixture is under one poll step"
        );
        let near = WaitDeadline::new(near_limit);
        // Read the clock *before* the call: `next_step` reads it at or after this
        // instant, so a step that fits inside `at - before` proves it fits inside the
        // real (never longer) remaining time it actually measured.
        let before = Instant::now();
        let step = near
            .next_step()
            .expect("a deadline 200ms out has not elapsed yet");
        assert!(
            step <= near_limit,
            "a nearer deadline shortens the sleep instead of overshooting it: {step:?}"
        );
        assert!(
            before + step <= near.at,
            "the scheduled sleep must not run past the deadline: {step:?}"
        );
    }

    /// A deadline that is already up yields no sleep at all — the signal [`run`]
    /// turns into the give-up exit instead of napping past the caller's `--timeout`.
    #[test]
    fn next_step_reports_an_expired_deadline() {
        let expired = WaitDeadline {
            limit: Duration::from_secs(1),
            // `next_step` reads the clock strictly after this, so this instant is
            // already reached by the time it looks.
            at: Instant::now(),
        };
        assert!(
            expired.next_step().is_none(),
            "a deadline already reached must not schedule another sleep"
        );
    }
}
