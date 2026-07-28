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
//! processkit-cli wait --all             [--timeout 10m]
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
//! # The three outcomes of `--run-id`
//!
//! Scoped to the single-`run_id` mode only — see "The aggregate barrier: `wait --all`"
//! below for why `--all` has just two outcomes, with no `CONTROL` among them.
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
//!
//! # The aggregate barrier: `wait --all`
//!
//! [`run_all`] is the counterpart for a caller that does not hold one `run_id` but
//! wants a barrier on *every* run — the typical orchestrator teardown sequence
//! (cancel everything → wait for it all to be gone → prune). It reuses the exact same
//! periodic-probing mechanism as [`run`], differing only in what it tracks:
//!
//! - **The target set is a snapshot, fixed once, at the moment the call starts** —
//!   exactly the entries [`registry::Registry::entries`] confirms
//!   [`registry::Health::Live`] at that instant. A run that registers *after* the
//!   snapshot is out of scope for this invocation and is never waited for. This is a
//!   deliberate, documented trade-off, not an oversight — one clear rule (the target
//!   set never grows) beats a plausible-sounding but unbounded alternative ("keep
//!   discovering new runs forever"), which would leave a caller unable to say when
//!   `--all` could ever return. A caller that wants to catch a run starting
//!   concurrently with the wait re-issues `wait --all` once this one returns. See
//!   `docs/registry.md`, "Waiting — `wait`".
//! - **The same "confirmed live" bar applies to the snapshot itself**, so an entry
//!   that is [`registry::Health::Unprobed`] — not confirmed live — at the exact
//!   instant the snapshot is taken is excluded from the target set outright: it is
//!   never tracked, and its outcome never affects `--all` at all. This is a
//!   deliberate, documented asymmetry with `--run-id`, which starts already knowing
//!   the one id to track and so has no equivalent "exclude before tracking begins"
//!   step of its own. See [`snapshot_target_paths`] and `docs/registry.md`, "Waiting —
//!   `wait`".
//! - **Once an entry is in the target set, it stays outstanding for as long as it
//!   cannot be confirmed over** — every pass *after* the snapshot applies the exact
//!   same conservative stance the single-run case above takes:
//!   [`RunStatus::Unprobed`]'s "unknown is not confirmed" rule, per entry, on every
//!   later re-probe. [`reprobe_targets`] never silently drops an entry it could not
//!   re-probe from the target set.
//! - Success (`0`) means every snapshot entry probed stale or vanished from the
//!   registry entirely (the same two indistinguishable "over" observations
//!   [`RunStatus::Finished`] already folds into one case). There is no aggregate
//!   `Ambiguous` outcome — `--all` never resolves an id at all, so the duplicate-id
//!   question [`RunStatus::Ambiguous`] answers for `--run-id` does not arise.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::control;
use crate::duration_fmt::format_duration;
use crate::exit::{self, RunnerError};
use crate::registry::{self, Health, RunStatus};

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
    let registry = registry::open_read_only_for_setup()?;
    let deadline = timeout.map(WaitDeadline::new);

    loop {
        let status = registry
            .probe_run(run_id)
            .map_err(registry::setup_read_error)?;
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

/// Run `wait --all [--timeout <duration>]`: poll the per-user registry until none of
/// the runs confirmed live in a snapshot taken at the moment this call starts are
/// still live, then exit `0` — the aggregate counterpart to [`run`]'s single-`run_id`
/// barrier. See the module doc's "The aggregate barrier: `wait --all`" section for the
/// snapshot and unprobed-entry semantics this implements.
///
/// Each snapshot entry is tracked by its record file path ([`registry::Entry::path`]),
/// not by `run_id`: two entries can share a `run_id` (the registry never enforces
/// uniqueness, `docs/registry.md`, "Run id resolution"), and this barrier's job is
/// "every *entry* confirmed live at the snapshot", not "every distinct id" — unlike
/// [`run`], there is no per-id ambiguity question to ask here at all.
///
/// Structured exactly like [`run`]'s loop, just with a set of targets standing in for
/// the single `run_id`: a target's health is established **first** (the snapshot
/// before the loop, then [`reprobe_targets`] at the end of each iteration), the loop
/// body only ever *decides* from the health already established, and the last pass's
/// findings (`any_unprobed`) are what a timeout reports — never a status the loop
/// itself never actually observed.
pub fn run_all(timeout: Option<Duration>) -> Result<(), RunnerError> {
    let registry = registry::open_read_only_for_setup()?;
    let deadline = timeout.map(WaitDeadline::new);

    // The snapshot: one scan, fixed before the first poll, to exactly the entries
    // confirmed live right now. Every later pass only ever *removes* from this set —
    // nothing is ever added to it (see the module doc for why).
    let mut targets = snapshot_target_paths(&registry).map_err(registry::setup_read_error)?;
    // Whether the most recent pass over `targets` found an entry that could not be
    // re-probed — reported honestly on a timeout instead of a confident "still live"
    // the last pass never actually established for every remaining entry.
    let mut any_unprobed = false;

    loop {
        if targets.is_empty() {
            return Ok(());
        }

        match &deadline {
            None => sleep(POLL_INTERVAL),
            Some(deadline) => match deadline.next_step() {
                Some(step) => sleep(step),
                None => {
                    return Err(wait_all_timed_out(
                        targets.len(),
                        any_unprobed,
                        deadline.limit,
                    ));
                }
            },
        }

        let (next_targets, unprobed) =
            reprobe_targets(&registry, &targets).map_err(registry::setup_read_error)?;
        targets = next_targets;
        any_unprobed = unprobed;
    }
}

/// One registry scan, filtered to the entries [`Health::Live`] confirms live right
/// now — the snapshot [`run_all`] fixes its target set to before its first poll. An
/// entry [`Health::Unprobed`] at snapshot time is **not** included: the target set is
/// documented as exactly the entries *confirmed* live at that instant, not "anything
/// that might be" — see the module doc.
fn snapshot_target_paths(registry: &registry::Registry) -> std::io::Result<HashSet<PathBuf>> {
    Ok(registry
        .snapshot_live_entries()?
        .into_iter()
        .map(|entry| entry.path)
        .collect())
}

/// Re-probe every entry still in `targets` against a fresh scan, dropping any that is
/// confirmed over — [`Health::Stale`], or gone from the scan entirely (a clean exit
/// removes its own record, the same "no record" observation [`RunStatus::Finished`]
/// already folds into one case for the single-run barrier) — and keeping every one
/// that re-probes [`Health::Live`] or [`Health::Unprobed`] (an unprobeable entry is
/// never read as a completed run — the same stance [`RunStatus::Unprobed`] documents).
/// Returns the surviving set alongside whether any survivor is only
/// [`Health::Unprobed`], for [`wait_all_timed_out`] to report honestly.
fn reprobe_targets(
    registry: &registry::Registry,
    targets: &HashSet<PathBuf>,
) -> std::io::Result<(HashSet<PathBuf>, bool)> {
    let mut current: HashMap<PathBuf, Health> = registry
        .entries()?
        .into_iter()
        .map(|entry| (entry.path, entry.health))
        .collect();

    let mut still_outstanding = HashSet::with_capacity(targets.len());
    let mut any_unprobed = false;
    for path in targets {
        match current.remove(path) {
            Some(Health::Live) => {
                still_outstanding.insert(path.clone());
            }
            Some(Health::Unprobed) => {
                any_unprobed = true;
                still_outstanding.insert(path.clone());
            }
            // Confirmed stale, or no longer present in the scan at all: over either
            // way.
            Some(Health::Stale) | None => {}
        }
    }
    Ok((still_outstanding, any_unprobed))
}

/// The give-up error for [`run_all`]: `--timeout` elapsed with `outstanding` snapshot
/// entries not yet confirmed over. Worded like [`wait_timed_out`] — names *waiting* as
/// what stopped, never claims a run was ended — and, like it, never overstates what the
/// last pass actually established: `any_unprobed` says whether at least one outstanding
/// entry is only unconfirmed rather than affirmatively still live.
fn wait_all_timed_out(outstanding: usize, any_unprobed: bool, limit: Duration) -> RunnerError {
    let noun = if outstanding == 1 { "run" } else { "runs" };
    let observed = if any_unprobed {
        "at least one of them is not confirmed finished — a matching registry entry \
         could not be re-probed, and an unprobeable entry is never read as a completed run"
    } else {
        "all of them are still live and were left running"
    };
    RunnerError::new(
        exit::WAIT_TIMEOUT,
        format!(
            "stopped waiting for all runs after {}: {outstanding} snapshot {noun} still \
             outstanding — {observed} — raise or drop `--timeout` to keep waiting",
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

    /// The `--all` give-up error carries the same reserved `WAIT_TIMEOUT` code as the
    /// single-run one, names how many snapshot entries are still outstanding (with
    /// correct singular/plural wording), and — when nothing was left unprobeable —
    /// confidently says they are still live.
    #[test]
    fn wait_all_timed_out_uses_the_waiters_own_code_and_reports_the_outstanding_count() {
        let err = wait_all_timed_out(2, false, Duration::from_secs(5));
        assert_eq!(err.code(), exit::WAIT_TIMEOUT);
        assert_ne!(
            err.code(),
            exit::TIMEOUT,
            "a waiter's deadline must never be reported as any run's deadline"
        );
        let message = err.to_string();
        assert!(
            message.contains('2'),
            "names the outstanding count: {message}"
        );
        assert!(message.contains("runs"), "uses plural wording: {message}");
        assert!(
            message.contains("still live"),
            "states the runs outlived the wait: {message}"
        );
        assert!(
            message.contains("5s"),
            "echoes the requested deadline: {message}"
        );

        let singular = wait_all_timed_out(1, false, Duration::from_secs(5));
        assert!(
            singular.to_string().contains("1 snapshot run "),
            "uses singular wording for exactly one outstanding entry: {}",
            singular
        );
    }

    /// Giving up while at least one outstanding entry is only unprobeable reports
    /// that honestly, never a confident "still live" the last pass never established
    /// for every entry — the aggregate counterpart to
    /// `wait_timed_out_does_not_claim_liveness_it_never_confirmed`.
    #[test]
    fn wait_all_timed_out_does_not_claim_liveness_it_never_confirmed() {
        let err = wait_all_timed_out(3, true, Duration::from_secs(5));
        assert_eq!(err.code(), exit::WAIT_TIMEOUT);
        let message = err.to_string();
        assert!(
            message.contains("not confirmed finished")
                && message.contains("could not be re-probed"),
            "names the unprobeable entry as the reason: {message}"
        );
        assert!(
            !message.contains("still live"),
            "must not assert liveness the probe never confirmed: {message}"
        );
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

    /// A unique, empty scratch directory for a fixture registry, mirroring
    /// `src/registry/mod.rs`'s own `scratch` test helper — there is no cross-module test
    /// helper to share, so `wait`'s unit tests need their own copy.
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "processkit-cli-wait-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Hand-write a confirmed-stale entry directly into `dir`: a well-formed record
    /// plus an **unlocked** sibling lock file — mirrors `tests/registry.rs`'s
    /// `write_stale_entry` (no cross-target test helper exists to share). Returns the
    /// record's path, the same identity [`snapshot_target_paths`]/[`reprobe_targets`]
    /// track entries by.
    fn write_stale_entry(dir: &std::path::Path, stem: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("create the registry directory");
        let lock_name = format!("{stem}.lock");
        let record = format!(
            "{{\"registry_version\":1,\"run_id\":\"{stem}\",\"endpoint\":null,\
             \"started_at\":\"2026-07-22T00:00:00.000Z\",\
             \"liveness\":{{\"kind\":\"advisory_lock\",\"lock_file\":\"{lock_name}\"}}}}"
        );
        let json_path = dir.join(format!("{stem}.json"));
        std::fs::write(&json_path, record).expect("write the stale record");
        std::fs::write(dir.join(&lock_name), b"").expect("write the unlocked lock file");
        json_path
    }

    /// Hand-write an unprobeable entry (T-206 fixture) directly into `dir`: a
    /// well-formed record whose `lock_file` name resolves to a **directory** rather
    /// than a regular file, so the liveness probe's write-open fails with a semantic
    /// error on every platform and for every user — mirrors `tests/registry.rs`'s
    /// `write_unprobeable_entry`. Returns the record's path.
    fn write_unprobeable_entry(dir: &std::path::Path, stem: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("create the registry directory");
        let lock_name = format!("{stem}.lock");
        let record = format!(
            "{{\"registry_version\":1,\"run_id\":\"{stem}\",\"endpoint\":null,\
             \"started_at\":\"2026-07-22T00:00:00.000Z\",\
             \"liveness\":{{\"kind\":\"advisory_lock\",\"lock_file\":\"{lock_name}\"}}}}"
        );
        let json_path = dir.join(format!("{stem}.json"));
        std::fs::write(&json_path, record).expect("write the record");
        std::fs::create_dir(dir.join(&lock_name))
            .expect("create the directory the lock name resolves to");
        json_path
    }

    /// [`snapshot_target_paths`]'s projection, proved directly rather than only
    /// through `run_all`'s end-to-end behavior: a confirmed-`Health::Live` entry is in
    /// scope, while a confirmed-`Health::Stale` entry and — the R-02 asymmetry
    /// documented in the module doc above — an entry that is only `Health::Unprobed`
    /// *at the snapshot instant* are both excluded outright, never entering the target
    /// set at all.
    #[test]
    fn snapshot_target_paths_include_only_confirmed_live_entries() {
        let dir = scratch("snapshot");
        let registry = registry::Registry::open_in(dir.clone()).expect("open registry");

        let live = registry
            .register_plain("live-run", None, std::time::SystemTime::now())
            .expect("register a live run");
        let live_path = live.record_path().to_path_buf();

        let stale_path = write_stale_entry(&dir, "stale-run");
        let unprobed_path = write_unprobeable_entry(&dir, "unprobed-run");

        let snapshot = snapshot_target_paths(&registry).expect("scan the fixture registry");

        assert!(
            snapshot.contains(&live_path),
            "a confirmed-live entry is in the snapshot's target set"
        );
        assert!(
            !snapshot.contains(&stale_path),
            "a confirmed-stale entry is excluded from the snapshot"
        );
        assert!(
            !snapshot.contains(&unprobed_path),
            "an entry only unprobed at snapshot time is excluded outright, never \
             entering the target set at all"
        );
        assert_eq!(
            snapshot.len(),
            1,
            "exactly the confirmed-live entry is ever in scope"
        );

        live.remove();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [`reprobe_targets`]'s decision logic on entries already in the target set — the
    /// half of the barrier's honesty discipline [`snapshot_target_paths`]'s test above
    /// does not reach. A `Health::Live` survivor stays outstanding; a `Health::Stale`
    /// survivor, and one gone from the scan entirely, are both dropped as confirmed
    /// over; and — the explicit task criterion this proves — a `Health::Unprobed`
    /// survivor stays outstanding too, never silently dropped, and is reported through
    /// `any_unprobed` rather than folded into a confident "still live".
    #[test]
    fn reprobe_targets_keeps_live_and_unprobed_drops_stale_and_missing() {
        let dir = scratch("reprobe");
        let registry = registry::Registry::open_in(dir.clone()).expect("open registry");

        let live = registry
            .register_plain("live-run", None, std::time::SystemTime::now())
            .expect("register a live run");
        let live_path = live.record_path().to_path_buf();

        let stale_path = write_stale_entry(&dir, "stale-run");
        let unprobed_path = write_unprobeable_entry(&dir, "unprobed-run");
        // A target the fixture never wrote at all — the "vanished from the scan" case
        // a clean exit produces (its own record removed).
        let missing_path = dir.join("never-existed.json");

        let mut targets: HashSet<PathBuf> = HashSet::new();
        targets.insert(live_path.clone());
        targets.insert(stale_path.clone());
        targets.insert(unprobed_path.clone());
        targets.insert(missing_path.clone());

        let (surviving, any_unprobed) =
            reprobe_targets(&registry, &targets).expect("reprobe against the fixture registry");

        assert!(
            surviving.contains(&live_path),
            "a Live entry stays outstanding"
        );
        assert!(
            surviving.contains(&unprobed_path),
            "an Unprobed entry stays outstanding, never silently dropped"
        );
        assert!(
            !surviving.contains(&stale_path),
            "a Stale entry is dropped as confirmed over"
        );
        assert!(
            !surviving.contains(&missing_path),
            "an entry gone from the scan entirely is dropped as confirmed over"
        );
        assert_eq!(
            surviving.len(),
            2,
            "exactly the live and unprobed entries survive the pass"
        );
        assert!(
            any_unprobed,
            "the unprobed survivor is reported honestly, not folded into a confident \
             'still live'"
        );

        live.remove();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `any_unprobed` flag is `false` when every survivor is confirmed `Live` — the
    /// counterpart to the mixed case above, proving the flag is not simply always
    /// `true` once anything at all is outstanding.
    #[test]
    fn reprobe_targets_reports_no_unprobed_when_every_survivor_is_confirmed_live() {
        let dir = scratch("reprobe-all-live");
        let registry = registry::Registry::open_in(dir.clone()).expect("open registry");

        let live = registry
            .register_plain("live-run", None, std::time::SystemTime::now())
            .expect("register a live run");
        let live_path = live.record_path().to_path_buf();

        let mut targets: HashSet<PathBuf> = HashSet::new();
        targets.insert(live_path.clone());

        let (surviving, any_unprobed) =
            reprobe_targets(&registry, &targets).expect("reprobe against the fixture registry");

        assert!(surviving.contains(&live_path));
        assert!(
            !any_unprobed,
            "no survivor is unprobed, so the flag must not overclaim one is"
        );

        live.remove();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
