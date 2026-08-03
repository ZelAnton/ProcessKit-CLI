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
//! processkit-cli wait --run-id build-42 --report-outcome
//! processkit-cli wait --all             [--timeout 10m] [--report-outcome]
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
//! Scoped to the target's lifetime only — see "The aggregate barrier: `wait --all`"
//! below for the snapshot and report-array rules in aggregate mode.
//!
//! | Exit | Meaning |
//! | --- | --- |
//! | `0` | The run is over: its record is gone, or every record under that id probed as stale. |
//! | [`exit::WAIT_TIMEOUT`] (112) | `--timeout` elapsed while the run was still live. **The waiter** gave up; the run was untouched and is still going. |
//! | [`exit::CONTROL`] (103) | More than one live run is registered under that `run_id`, so there is no single run to wait for. |
//!
//! Nothing is printed on ordinary success: the exit code *is* the answer. The
//! `--report-outcome` opt-in prints one JSON object for `--run-id`, or one JSON array
//! for `--all`, after success, without changing that exit code. It remembers each
//! live record's JSONL locator before clean teardown deletes the record, then reads
//! the terminal `runner_exit`. A failure explains itself on stderr, like every other
//! subcommand.
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

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::Serialize;

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

/// Once a remembered live record disappears, its runner is in the tiny teardown
/// window between registry removal and the flushed terminal event. Retry only the
/// opt-in report for a short bounded period; an abruptly killed runner then yields
/// an honest unknown outcome instead of making `wait` hang.
const OUTCOME_SETTLE_TIMEOUT: Duration = Duration::from_millis(500);
const OUTCOME_RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// Only the tail can contain the terminal event. Bounding this read prevents a
/// large lifecycle stream from becoming `wait`'s memory cost. Exposed
/// `#[doc(hidden)] pub` so the fuzz tier's `runner_exit_tail` target
/// (`fuzz/fuzz_targets/runner_exit_tail.rs`, T-301) can derive the identical
/// head/tail window sizes from a simulated events file without duplicating (and
/// risking drifting from) this value.
#[doc(hidden)]
pub const OUTCOME_TAIL_MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WaitOutcomeStatus {
    Reported,
    Unknown,
}

/// One machine-readable entry emitted by `wait --report-outcome`. Null fields are
/// always present so a consumer never has to distinguish absent from unknown. The
/// single-run form emits this object directly; aggregate mode emits an array of them.
#[derive(Debug, PartialEq, Eq, Serialize)]
struct WaitOutcome<'a> {
    run_id: &'a str,
    status: WaitOutcomeStatus,
    code: Option<i32>,
    source: Option<String>,
    child_code: Option<i32>,
}

///
/// `#[doc(hidden)] pub` (fields stay module-private) purely so it can appear in
/// the return type of the `#[doc(hidden)] pub` fuzz-tier primitives below —
/// `pub fn`s cannot return a private type (`E0446`).
#[derive(Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct TerminalOutcome {
    code: i32,
    source: String,
    child_code: Option<i32>,
}

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
pub fn run(
    run_id: &str,
    timeout: Option<Duration>,
    report_outcome: bool,
) -> Result<(), RunnerError> {
    let registry = registry::open_read_only_for_setup()?;
    let deadline = timeout.map(WaitDeadline::new);
    let mut observed_live = false;
    let mut jsonl = None;

    loop {
        let probe = registry
            .probe_run_with_jsonl(run_id)
            .map_err(registry::setup_read_error)?;
        let status = probe.status;
        match status {
            RunStatus::Finished => {
                if report_outcome {
                    print_outcome(run_id, observed_live.then_some(jsonl).flatten())?;
                }
                return Ok(());
            }
            RunStatus::Ambiguous { live } => {
                return Err(control::ambiguous_run("wait for", run_id, live));
            }
            RunStatus::Live => {
                observed_live = true;
                if jsonl.is_none() {
                    jsonl = probe.jsonl;
                }
            }
            // Still going, or not confirmed over — either way, keep waiting.
            RunStatus::Unprobed => {}
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

fn outcome_for<'a>(run_id: &'a str, jsonl: Option<&std::path::Path>) -> WaitOutcome<'a> {
    let terminal = jsonl.and_then(|path| await_terminal_outcome(path, run_id));
    match terminal {
        Some(terminal) => WaitOutcome {
            run_id,
            status: WaitOutcomeStatus::Reported,
            code: Some(terminal.code),
            source: Some(terminal.source),
            child_code: terminal.child_code,
        },
        None => WaitOutcome {
            run_id,
            status: WaitOutcomeStatus::Unknown,
            code: None,
            source: None,
            child_code: None,
        },
    }
}

fn print_outcome(run_id: &str, jsonl: Option<PathBuf>) -> Result<(), RunnerError> {
    let report = outcome_for(run_id, jsonl.as_deref());
    let line = serde_json::to_string(&report).map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not render the wait outcome report: {err}"),
        )
    })?;
    println!("{line}");
    Ok(())
}

/// Read the terminal event after the live record disappears. Normal teardown
/// removes the record immediately before emitting `runner_exit`, so retry across
/// that bounded handoff; an absent/malformed stream after the window is simply an
/// unknown outcome, not a different wait exit code.
fn await_terminal_outcome(path: &std::path::Path, run_id: &str) -> Option<TerminalOutcome> {
    if !path.is_absolute() {
        return None;
    }
    let deadline = Instant::now() + OUTCOME_SETTLE_TIMEOUT;
    loop {
        if let Ok(Some(outcome)) = read_terminal_outcome(path, run_id) {
            return Some(outcome);
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(OUTCOME_RETRY_INTERVAL);
    }
}

fn read_terminal_outcome(
    path: &std::path::Path,
    expected_run_id: &str,
) -> std::io::Result<Option<TerminalOutcome>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut head = Vec::with_capacity(len.min(OUTCOME_TAIL_MAX_BYTES) as usize);
    (&mut file)
        .take(OUTCOME_TAIL_MAX_BYTES)
        .read_to_end(&mut head)?;
    if !head_matches_run_id(&head, expected_run_id) {
        return Ok(None);
    }

    let start = len.saturating_sub(OUTCOME_TAIL_MAX_BYTES);
    file.seek(SeekFrom::Start(start))?;

    let mut tail = Vec::with_capacity((len - start).min(OUTCOME_TAIL_MAX_BYTES) as usize);
    file.take(OUTCOME_TAIL_MAX_BYTES).read_to_end(&mut tail)?;
    Ok(scan_runner_exit_tail(&tail, start == 0))
}

/// Whether the stream's head window contains a `run_started` line naming
/// `expected_run_id` — the read-back path's first gate, confirming the events
/// file actually belongs to the run being asked about before it bothers scanning
/// the tail at all (a non-matching head means [`read_terminal_outcome`] returns
/// `None` without ever reading the tail). `head` is the stream's first
/// `min(len, OUTCOME_TAIL_MAX_BYTES)` bytes — exactly what
/// [`read_terminal_outcome`] itself reads into its own `head`. Pure — no I/O —
/// which, together with [`scan_runner_exit_tail`], is what lets the read-back
/// path be driven directly with arbitrary bytes standing in for a whole events
/// file: the fuzz tier's `runner_exit_tail` target
/// (`fuzz/fuzz_targets/runner_exit_tail.rs`, T-301), by the same
/// `#[doc(hidden)] pub` exposure pattern `registry_record`/`control_wire`/
/// `cli_parsers` already use (K-041/K-060) — and any future consumer of the same
/// tail-read-back primitive.
///
/// That fuzz target is, as of the `events` subcommand landing, still the only
/// other consumer: `events` reads a stream a different way on purpose — walking it
/// incrementally and handing out complete lines as the file grows, rather than
/// scanning a bounded head/tail window for one terminal event — so it does *not*
/// route through this function (see [`crate::events_cmd`]). This doc comment used
/// to name it as a prospective caller; it is not one.
#[doc(hidden)]
pub fn head_matches_run_id(head: &[u8], expected_run_id: &str) -> bool {
    head.split(|byte| *byte == b'\n').any(|line| {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            return false;
        };
        value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            == Some(u64::from(crate::events::SCHEMA_VERSION))
            && value.get("event").and_then(serde_json::Value::as_str) == Some("run_started")
            && value.get("run_id").and_then(serde_json::Value::as_str) == Some(expected_run_id)
    })
}

/// Scan a stream's tail window, in reverse, for its last well-formed terminal
/// `runner_exit` line — the read-back path's second step, only ever reached by
/// [`read_terminal_outcome`] once [`head_matches_run_id`] has already confirmed
/// the stream. `tail` is the stream's last `min(len, OUTCOME_TAIL_MAX_BYTES)`
/// bytes — exactly what [`read_terminal_outcome`] itself reads into its own
/// `tail`. `tail_is_file_start` is whether that window's first byte is byte 0 of
/// the real stream: `true` means the window's first line is complete; `false`
/// means the window was sought into the middle of a larger stream, so its first
/// line is necessarily partial (its own start was truncated by the seek) and
/// must be dropped before scanning — the same distinction
/// [`read_terminal_outcome`]'s own `start == 0` check makes. Pure — no I/O — see
/// [`head_matches_run_id`] for why that is what lets both double as the fuzz
/// tier's `runner_exit_tail` target primitives.
#[doc(hidden)]
pub fn scan_runner_exit_tail(tail: &[u8], tail_is_file_start: bool) -> Option<TerminalOutcome> {
    let usable = if tail_is_file_start {
        tail
    } else {
        let index = tail.iter().position(|byte| *byte == b'\n')?;
        &tail[index + 1..]
    };

    for line in usable.split(|byte| *byte == b'\n').rev() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("event").and_then(serde_json::Value::as_str) != Some("runner_exit") {
            continue;
        }
        if value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            != Some(u64::from(crate::events::SCHEMA_VERSION))
        {
            continue;
        }
        let Some(code) = value
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .and_then(|code| i32::try_from(code).ok())
        else {
            continue;
        };
        let Some(source) = value
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let child_code = match value.get("child_code") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => {
                let Some(code) = value.as_i64().and_then(|code| i32::try_from(code).ok()) else {
                    continue;
                };
                Some(code)
            }
        };
        return Some(TerminalOutcome {
            code,
            source,
            child_code,
        });
    }
    None
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
pub fn run_all(
    timeout: Option<Duration>,
    labels: &[crate::labels::OperatorLabel],
    report_outcome: bool,
) -> Result<(), RunnerError> {
    let registry = registry::open_read_only_for_setup()?;
    let deadline = timeout.map(WaitDeadline::new);

    // The snapshot: one scan, fixed before the first poll, to exactly the entries
    // confirmed live right now. Every later pass only ever *removes* from this set —
    // nothing is ever added to it (see the module doc for why).
    let snapshot_targets =
        snapshot_target_paths(&registry, labels).map_err(registry::setup_read_error)?;
    let mut targets = snapshot_targets.clone();
    // Whether the most recent pass over `targets` found an entry that could not be
    // re-probed — reported honestly on a timeout instead of a confident "still live"
    // the last pass never actually established for every remaining entry.
    let mut any_unprobed = false;

    loop {
        if targets.is_empty() {
            if report_outcome {
                print_all_outcomes(&snapshot_targets)?;
            }
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
fn snapshot_target_paths(
    registry: &registry::Registry,
    labels: &[crate::labels::OperatorLabel],
) -> std::io::Result<Vec<WaitTarget>> {
    let mut targets: Vec<WaitTarget> = registry
        .snapshot_live_entries()?
        .into_iter()
        .filter(|entry| crate::labels::matches(&entry.record.labels, labels))
        .map(|entry| WaitTarget {
            run_id: entry.record.run_id,
            record_path: entry.path,
            jsonl: entry.record.jsonl.map(PathBuf::from),
        })
        .collect();
    targets.sort_by(|left, right| {
        left.run_id
            .cmp(&right.run_id)
            .then_with(|| left.record_path.cmp(&right.record_path))
    });
    Ok(targets)
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
    targets: &[WaitTarget],
) -> std::io::Result<(Vec<WaitTarget>, bool)> {
    let mut current: HashMap<PathBuf, Health> = registry
        .entries()?
        .into_iter()
        .map(|entry| (entry.path, entry.health))
        .collect();

    let mut still_outstanding = Vec::with_capacity(targets.len());
    let mut any_unprobed = false;
    for target in targets {
        match current.remove(&target.record_path) {
            Some(Health::Live) => {
                still_outstanding.push(target.clone());
            }
            Some(Health::Unprobed) => {
                any_unprobed = true;
                still_outstanding.push(target.clone());
            }
            // Confirmed stale, or no longer present in the scan at all: over either
            // way.
            Some(Health::Stale) | None => {}
        }
    }
    Ok((still_outstanding, any_unprobed))
}

#[derive(Debug, Clone)]
struct WaitTarget {
    run_id: String,
    record_path: PathBuf,
    jsonl: Option<PathBuf>,
}

/// Print one outcome entry for every target in the original snapshot, in the same
/// deterministic order used by the barrier. The report deliberately uses the
/// snapshot's locators rather than looking up records after the barrier: a clean
/// runner removes its registry record before the terminal event is flushed, and a
/// replacement record must never retarget a report for the old run.
fn print_all_outcomes(targets: &[WaitTarget]) -> Result<(), RunnerError> {
    let report: Vec<WaitOutcome<'_>> = targets
        .iter()
        .map(|target| outcome_for(&target.run_id, target.jsonl.as_deref()))
        .collect();
    let line = serde_json::to_string(&report).map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not render the wait --all outcome report: {err}"),
        )
    })?;
    println!("{line}");
    Ok(())
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
    use crate::registry::test_support::{
        scratch_registry as scratch, write_stale_entry, write_unprobeable_entry,
    };

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

    #[test]
    fn terminal_outcome_reader_finds_the_last_runner_exit_in_a_bounded_tail() {
        let dir = scratch("wait-outcome-tail");
        let path = dir.join("events.jsonl");
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let mut contents =
            br#"{"schema_version":1,"event":"run_started","run_id":"run-7"}"#.to_vec();
        contents.push(b'\n');
        contents.extend(vec![b'x'; OUTCOME_TAIL_MAX_BYTES as usize + 1024]);
        contents.push(b'\n');
        contents.extend_from_slice(
            br#"{"schema_version":1,"event":"runner_exit","code":7,"source":"child_exit","child_code":7}"#,
        );
        contents.push(b'\n');
        std::fs::write(&path, contents).expect("write the JSONL fixture");

        let outcome = read_terminal_outcome(&path, "run-7")
            .expect("read the bounded tail")
            .expect("find runner_exit");
        assert_eq!(
            outcome,
            TerminalOutcome {
                code: 7,
                source: "child_exit".to_string(),
                child_code: Some(7),
            }
        );
        assert!(
            read_terminal_outcome(&path, "different-run")
                .expect("read the same stream")
                .is_none(),
            "a reused locator cannot be attributed to a different run id"
        );

        std::fs::write(
            &path,
            concat!(
                "{\"schema_version\":1,\"event\":\"run_started\",\"run_id\":\"run-7\"}\n",
                "{\"schema_version\":1,\"event\":\"runner_exit\",\"code\":7,",
                "\"source\":\"child_exit\",\"child_code\":\"seven\"}\n"
            ),
        )
        .expect("replace the fixture with a malformed terminal event");
        assert!(
            read_terminal_outcome(&path, "run-7")
                .expect("read the malformed stream")
                .is_none(),
            "invalid terminal field types must yield unknown, never reported"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_outcome_keeps_unknown_fields_explicitly_null() {
        let report = WaitOutcome {
            run_id: "already-gone",
            status: WaitOutcomeStatus::Unknown,
            code: None,
            source: None,
            child_code: None,
        };
        let value = serde_json::to_value(report).expect("serialize the report");
        assert_eq!(value["status"], "unknown");
        assert!(value["code"].is_null());
        assert!(value["source"].is_null());
        assert!(value["child_code"].is_null());
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

        let stale_path = write_stale_entry(&dir, "stale-run", "stale-run");
        let unprobed_path = write_unprobeable_entry(&dir, "unprobed-run", "unprobed-run");

        let snapshot = snapshot_target_paths(&registry, &[]).expect("scan the fixture registry");

        assert!(
            snapshot
                .iter()
                .any(|target| target.record_path == live_path),
            "a confirmed-live entry is in the snapshot's target set"
        );
        assert!(
            !snapshot
                .iter()
                .any(|target| target.record_path == stale_path),
            "a confirmed-stale entry is excluded from the snapshot"
        );
        assert!(
            !snapshot
                .iter()
                .any(|target| target.record_path == unprobed_path),
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

        let stale_path = write_stale_entry(&dir, "stale-run", "stale-run");
        let unprobed_path = write_unprobeable_entry(&dir, "unprobed-run", "unprobed-run");
        // A target the fixture never wrote at all — the "vanished from the scan" case
        // a clean exit produces (its own record removed).
        let missing_path = dir.join("never-existed.json");

        let targets = vec![
            WaitTarget {
                run_id: "live-run".to_string(),
                record_path: live_path.clone(),
                jsonl: None,
            },
            WaitTarget {
                run_id: "stale-run".to_string(),
                record_path: stale_path.clone(),
                jsonl: None,
            },
            WaitTarget {
                run_id: "unprobed-run".to_string(),
                record_path: unprobed_path.clone(),
                jsonl: None,
            },
            WaitTarget {
                run_id: "missing-run".to_string(),
                record_path: missing_path.clone(),
                jsonl: None,
            },
        ];

        let (surviving, any_unprobed) =
            reprobe_targets(&registry, &targets).expect("reprobe against the fixture registry");

        assert!(
            surviving
                .iter()
                .any(|target| target.record_path == live_path),
            "a Live entry stays outstanding"
        );
        assert!(
            surviving
                .iter()
                .any(|target| target.record_path == unprobed_path),
            "an Unprobed entry stays outstanding, never silently dropped"
        );
        assert!(
            !surviving
                .iter()
                .any(|target| target.record_path == stale_path),
            "a Stale entry is dropped as confirmed over"
        );
        assert!(
            !surviving
                .iter()
                .any(|target| target.record_path == missing_path),
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

        let targets = vec![WaitTarget {
            run_id: "live-run".to_string(),
            record_path: live_path.clone(),
            jsonl: None,
        }];

        let (surviving, any_unprobed) =
            reprobe_targets(&registry, &targets).expect("reprobe against the fixture registry");

        assert!(
            surviving
                .iter()
                .any(|target| target.record_path == live_path)
        );
        assert!(
            !any_unprobed,
            "no survivor is unprobed, so the flag must not overclaim one is"
        );

        live.remove();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
