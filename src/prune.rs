//! `prune`: reap the per-user registry's confirmed-stale entries.
//!
//! When a runner dies abruptly (crash, `SIGKILL`, a parent's Job Object terminate)
//! its clean-exit teardown never runs, so the `.json`/`.lock` pair it registered is
//! left behind (see [`registry::Registry::register`] / [`registry::Registration::remove`]).
//! `list` surfaces such a leftover as a `stale` entry; `prune` is the counterpart that
//! *removes* it, so the registry directory does not accumulate dead records that slow
//! scans and clutter diagnostics.
//!
//! The whole safety of this command lives in [`registry::Registry::prune`]: it deletes
//! **only** an entry whose liveness probe *succeeded and returned stale*, never a live
//! entry and never one whose probe merely failed (liveness unknown ⇒ left in place),
//! and it never addresses an entry by PID — it reaps through the record path the scan
//! produced. It also reaps a second, rarer kind of leftover the same way: a lone
//! `.lock` file whose `.json` never landed (or whose `.json` was removed but its
//! `.lock` delete failed), tallied separately as `orphaned_locks` since it deletes
//! one file, not a `.json`/`.lock` pair. On unix it reaps a third (T-207): the private
//! control-socket directory a confirmed-stale record published, the other half of what
//! an abrupt death leaks — validated by shape first, since the record's endpoint is
//! untrusted data, and counted under that record's own `pruned` tally rather than a
//! counter of its own (the socket belongs to the entry being reaped, and cannot be
//! reaped without it). This module is only the thin CLI wrapper: it opens the
//! registry, calls `prune` (or, under `--dry-run`, `preview_prune`), and reports the
//! tally.
//!
//! Like `list`, `prune` opens the registry through
//! [`registry::Registry::open_read_only`] — **not** the mutating [`registry::Registry::open`]
//! `run` uses. Prune does mutate the registry (it deletes files), but it must not
//! *create* the directory or re-assert its permissions just to reap: a missing or empty
//! registry simply has nothing to prune. That keeps prune from conjuring registry state
//! as a side effect, exactly as a read-only `list` must not.
//!
//! `--dry-run` (T-199) is the non-destructive preview of that same operation: it opens
//! the registry the same read-only way and calls
//! [`registry::Registry::preview_prune`] instead of [`registry::Registry::prune`] —
//! the exact same scan and the exact same liveness classification, but never a
//! `fs::remove_file`. The non-`--dry-run` output (both human-readable and `--json`)
//! is unchanged byte-for-byte; `--dry-run` only adds a new, separate output shape.

use serde::Serialize;

use crate::exit::{self, RunnerError};
use crate::registry::{self, PruneCandidate, PruneOutcome, PrunePreview};

/// The prune tally as printed for `--json` — a display shape decoupled from the
/// internal [`registry::PruneOutcome`] so the serialized field names are a stable CLI
/// contract, the same decoupling `list` uses for its rows.
#[derive(Debug, Serialize)]
struct PruneReport {
    /// Confirmed-stale entries (`.json`/`.lock` pairs) whose files were reaped.
    pruned: usize,
    /// Live entries left untouched — paired records and lone orphaned lock files
    /// alike.
    live: usize,
    /// Entries whose liveness could not be probed and were left in place — paired
    /// records and lone orphaned lock files alike.
    unprobed: usize,
    /// Confirmed-stale orphaned `.lock` files (no paired `.json`) that were reaped.
    /// Kept as its own field rather than folded into `pruned`, since a pruned entry
    /// deletes a `.json`/`.lock` pair while an orphaned-lock reap deletes only the
    /// one `.lock` file — see [`registry::PruneOutcome::orphaned_locks`].
    orphaned_locks: usize,
}

impl From<PruneOutcome> for PruneReport {
    fn from(outcome: PruneOutcome) -> Self {
        Self {
            pruned: outcome.pruned,
            live: outcome.live,
            unprobed: outcome.unprobed,
            orphaned_locks: outcome.orphaned_locks,
        }
    }
}

/// One [`registry::PruneCandidate`] as printed/serialized under `--dry-run` — a
/// display shape decoupled from the registry-internal enum for the same reason
/// [`PruneReport`] is decoupled from [`registry::PruneOutcome`]: stable, documented
/// field names are a CLI contract independent of how the internal type happens to be
/// shaped. Serialized internally tagged on `kind` (`"entry"` / `"orphaned_lock"`), so
/// a consumer branches on one field rather than guessing which of two disjoint field
/// sets is populated.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PruneCandidateReport {
    /// A paired `.json`/`.lock` record a real prune would reap.
    Entry {
        /// The record's `run_id`.
        run_id: String,
        /// The record's `started_at`, RFC 3339 UTC with millisecond precision.
        started_at: String,
        /// The private control-socket directory a real prune would reap along with
        /// this entry's two files (T-207), or `null` when it would reap none — the
        /// record published no endpoint, published one that is not the shape a
        /// control server creates (and so is refused rather than deleted), or is a
        /// Windows record, whose named-pipe endpoint leaves nothing on disk. Always
        /// present, so a consumer reads one field rather than inferring from its
        /// absence — see [`registry::PruneCandidate::Entry::socket_dir`].
        socket_dir: Option<String>,
    },
    /// A lone `.lock` file with no `.json` sibling a real prune would reap.
    OrphanedLock {
        /// The lock file's name (no directory component).
        lock_file_name: String,
    },
}

impl From<PruneCandidate> for PruneCandidateReport {
    fn from(candidate: PruneCandidate) -> Self {
        match candidate {
            PruneCandidate::Entry {
                run_id,
                started_at,
                socket_dir,
            } => Self::Entry {
                run_id,
                started_at,
                socket_dir,
            },
            PruneCandidate::OrphanedLock { lock_file_name } => {
                Self::OrphanedLock { lock_file_name }
            }
        }
    }
}

/// The `--dry-run` report printed under `--json`: the exact same aggregate fields
/// [`PruneReport`] carries — so a consumer already parsing `prune --json` needs no
/// new field names for the counts it already reads — plus `candidates`, the
/// machine-readable list of confirmed-stale entries a real prune would reap.
#[derive(Debug, Serialize)]
struct PruneDryRunReport {
    /// Confirmed-stale entries (`.json`/`.lock` pairs) a real prune would reap.
    pruned: usize,
    /// Live entries a real prune would leave untouched — paired records and lone
    /// orphaned lock files alike.
    live: usize,
    /// Entries whose liveness could not be probed and a real prune would leave in
    /// place — paired records and lone orphaned lock files alike.
    unprobed: usize,
    /// Confirmed-stale orphaned `.lock` files (no paired `.json`) a real prune would
    /// reap.
    orphaned_locks: usize,
    /// Every confirmed-stale candidate the counts above tally — what a real prune
    /// pass over this exact state would actually reap.
    candidates: Vec<PruneCandidateReport>,
}

impl From<PrunePreview> for PruneDryRunReport {
    fn from(preview: PrunePreview) -> Self {
        Self {
            pruned: preview.outcome.pruned,
            live: preview.outcome.live,
            unprobed: preview.outcome.unprobed,
            orphaned_locks: preview.outcome.orphaned_locks,
            candidates: preview
                .candidates
                .into_iter()
                .map(PruneCandidateReport::from)
                .collect(),
        }
    }
}

/// Run `prune [--json] [--dry-run]`: open the per-user registry read-only and either
/// reap every confirmed-stale entry (the default) or, under `--dry-run`, only preview
/// what that reap would do — reporting the tally as a human-readable summary line by
/// default, or as JSON with `--json`. The two output shapes are entirely separate:
/// `--dry-run`'s JSON object carries an additional `candidates` field the plain
/// `prune --json` object does not, and the non-`--dry-run` output is unchanged.
///
/// Uses [`registry::Registry::open_read_only`], not [`registry::Registry::open`]:
/// prune must never create the registry directory or touch its permissions just to
/// look at it, whether it goes on to reap or only preview (see the module docs
/// above). A missing or empty registry has nothing to prune or preview and exits `0`
/// either way; the only failure is the registry directory itself being unreadable, an
/// [`exit::SETUP`] condition (a support/prerequisite failure).
pub fn run(json: bool, dry_run: bool) -> Result<(), RunnerError> {
    let registry = registry::Registry::open_read_only().map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not open the run registry: {err}"),
        )
    })?;

    if dry_run {
        let preview = registry.preview_prune().map_err(|err| {
            RunnerError::new(
                exit::SETUP,
                format!("could not read the run registry: {err}"),
            )
        })?;
        if json {
            print_dry_run_json(preview)
        } else {
            print_dry_run_summary(&preview);
            Ok(())
        }
    } else {
        let outcome = registry.prune().map_err(|err| {
            RunnerError::new(
                exit::SETUP,
                format!("could not read the run registry: {err}"),
            )
        })?;
        if json {
            print_json(outcome)
        } else {
            print_summary(outcome);
            Ok(())
        }
    }
}

/// Print the tally as a single JSON object — the machine-readable form an
/// orchestrator parses.
fn print_json(outcome: PruneOutcome) -> Result<(), RunnerError> {
    let report = PruneReport::from(outcome);
    let line = serde_json::to_string(&report).map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not render the prune report: {err}"),
        )
    })?;
    println!("{line}");
    Ok(())
}

/// Print a concise, human-readable summary line: how many stale entries (and
/// orphaned lock files) were reaped, and — when any were kept back — how many live
/// and how many unprobeable ones were deliberately left alone.
fn print_summary(outcome: PruneOutcome) {
    if outcome.pruned == 0
        && outcome.live == 0
        && outcome.unprobed == 0
        && outcome.orphaned_locks == 0
    {
        println!("no stale entries to prune");
        return;
    }
    println!(
        "pruned {} stale ({} orphaned locks), kept {} live, left {} unprobeable",
        outcome.pruned, outcome.orphaned_locks, outcome.live, outcome.unprobed
    );
}

/// Print `--dry-run`'s tally and candidate list as a single JSON object — the
/// machine-readable form an orchestrator parses to learn exactly what a real prune
/// pass would reap before running one.
fn print_dry_run_json(preview: PrunePreview) -> Result<(), RunnerError> {
    let report = PruneDryRunReport::from(preview);
    let line = serde_json::to_string(&report).map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not render the prune dry-run report: {err}"),
        )
    })?;
    println!("{line}");
    Ok(())
}

/// Print `--dry-run`'s human-readable form: one line per confirmed-stale candidate
/// (a paired record's `run_id`/`started_at`, or an orphaned lock's file name), then
/// the same summary line shape [`print_summary`] prints, prefixed "would" throughout
/// since nothing was actually reaped.
///
/// A paired entry whose published control socket a real prune would reap too (T-207)
/// names that directory in a trailing `socket_dir=<path>` field; an entry with no
/// such leftover — no endpoint, an endpoint the reap would refuse, or a Windows
/// named pipe — simply omits the field rather than printing an empty value.
fn print_dry_run_summary(preview: &PrunePreview) {
    let outcome = preview.outcome;
    if outcome.pruned == 0
        && outcome.live == 0
        && outcome.unprobed == 0
        && outcome.orphaned_locks == 0
    {
        println!("no stale entries to prune (dry run)");
        return;
    }
    for candidate in &preview.candidates {
        match candidate {
            PruneCandidate::Entry {
                run_id,
                started_at,
                socket_dir,
            } => {
                let socket = socket_dir
                    .as_ref()
                    .map(|dir| format!(" socket_dir={dir}"))
                    .unwrap_or_default();
                println!("would prune entry run_id={run_id} started_at={started_at}{socket}");
            }
            PruneCandidate::OrphanedLock { lock_file_name } => {
                println!("would prune orphaned lock {lock_file_name}");
            }
        }
    }
    println!(
        "would prune {} stale ({} orphaned locks), keep {} live, leave {} unprobeable",
        outcome.pruned, outcome.orphaned_locks, outcome.live, outcome.unprobed
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JSON report carries the exact field names a consumer of `prune --json`
    /// depends on, and the counts pass through from the registry outcome verbatim.
    #[test]
    fn prune_report_serializes_the_documented_fields() {
        let report = PruneReport::from(PruneOutcome {
            pruned: 2,
            live: 1,
            unprobed: 3,
            orphaned_locks: 4,
        });
        let json = serde_json::to_string(&report).expect("a prune report serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["pruned"], 2);
        assert_eq!(value["live"], 1);
        assert_eq!(value["unprobed"], 3);
        assert_eq!(value["orphaned_locks"], 4);
    }

    /// T-199: the `--dry-run --json` report carries the exact same aggregate field
    /// names the plain `prune --json` report does — `pruned`/`live`/`unprobed`/
    /// `orphaned_locks` — plus a `candidates` array whose entries are tagged by
    /// `kind` and carry the documented identifying fields for each candidate shape.
    /// T-207 added `socket_dir` to the `entry` shape: the control-socket directory a
    /// real prune would reap along with the record.
    #[test]
    fn prune_dry_run_report_serializes_the_documented_fields() {
        let report = PruneDryRunReport::from(PrunePreview {
            outcome: PruneOutcome {
                pruned: 1,
                live: 2,
                unprobed: 3,
                orphaned_locks: 1,
            },
            candidates: vec![
                PruneCandidate::Entry {
                    run_id: "run-a".to_string(),
                    started_at: "2026-07-22T00:00:00.000Z".to_string(),
                    socket_dir: Some("/tmp/pkc-1234-abc-0".to_string()),
                },
                PruneCandidate::OrphanedLock {
                    lock_file_name: "orphan.lock".to_string(),
                },
            ],
        });
        let json = serde_json::to_string(&report).expect("a dry-run report serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["pruned"], 1);
        assert_eq!(value["live"], 2);
        assert_eq!(value["unprobed"], 3);
        assert_eq!(value["orphaned_locks"], 1);

        let candidates = value["candidates"]
            .as_array()
            .expect("candidates is a JSON array");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0]["kind"], "entry");
        assert_eq!(candidates[0]["run_id"], "run-a");
        assert_eq!(candidates[0]["started_at"], "2026-07-22T00:00:00.000Z");
        assert_eq!(candidates[0]["socket_dir"], "/tmp/pkc-1234-abc-0");
        assert_eq!(candidates[1]["kind"], "orphaned_lock");
        assert_eq!(candidates[1]["lock_file_name"], "orphan.lock");
    }

    /// T-207: an entry candidate whose reap would remove no control socket — no
    /// endpoint, an endpoint the reap refuses, or a Windows named pipe — still
    /// carries the `socket_dir` field, explicitly `null`, so a consumer branches on
    /// one always-present field instead of on a missing key. The human-readable line
    /// for the same candidate omits the field entirely rather than printing an empty
    /// value.
    #[test]
    fn prune_dry_run_report_serializes_a_null_socket_dir_for_an_entry_without_one() {
        let report = PruneDryRunReport::from(PrunePreview {
            outcome: PruneOutcome {
                pruned: 1,
                ..PruneOutcome::default()
            },
            candidates: vec![PruneCandidate::Entry {
                run_id: "run-a".to_string(),
                started_at: "2026-07-22T00:00:00.000Z".to_string(),
                socket_dir: None,
            }],
        });
        let json = serde_json::to_string(&report).expect("a dry-run report serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let candidate = &value["candidates"]
            .as_array()
            .expect("candidates is a JSON array")[0];
        assert!(
            candidate.get("socket_dir").is_some(),
            "socket_dir is always present, even with nothing to reap: {value}"
        );
        assert!(
            candidate["socket_dir"].is_null(),
            "an entry with no reapable socket reports socket_dir null: {value}"
        );
    }

    /// A dry run over an empty registry reports an empty `candidates` array — not a
    /// missing field — alongside the all-zero tally.
    #[test]
    fn prune_dry_run_report_serializes_an_empty_candidate_list() {
        let report = PruneDryRunReport::from(PrunePreview::default());
        let json = serde_json::to_string(&report).expect("a dry-run report serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["pruned"], 0);
        assert!(
            value["candidates"].as_array().expect("an array").is_empty(),
            "an empty preview serializes an empty candidates array: {value}"
        );
    }
}
