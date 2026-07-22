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
//! produced. This module is only the thin CLI wrapper: it opens the registry, calls
//! `prune`, and reports the tally.
//!
//! Like `list`, `prune` opens the registry through
//! [`registry::Registry::open_read_only`] — **not** the mutating [`registry::Registry::open`]
//! `run` uses. Prune does mutate the registry (it deletes files), but it must not
//! *create* the directory or re-assert its permissions just to reap: a missing or empty
//! registry simply has nothing to prune. That keeps prune from conjuring registry state
//! as a side effect, exactly as a read-only `list` must not.

use serde::Serialize;

use crate::exit::{self, RunnerError};
use crate::registry::{self, PruneOutcome};

/// The prune tally as printed for `--json` — a display shape decoupled from the
/// internal [`registry::PruneOutcome`] so the serialized field names are a stable CLI
/// contract, the same decoupling `list` uses for its rows.
#[derive(Debug, Serialize)]
struct PruneReport {
    /// Confirmed-stale entries whose files were reaped.
    pruned: usize,
    /// Live entries left untouched.
    live: usize,
    /// Entries whose liveness could not be probed and were left in place.
    unprobed: usize,
}

impl From<PruneOutcome> for PruneReport {
    fn from(outcome: PruneOutcome) -> Self {
        Self {
            pruned: outcome.pruned,
            live: outcome.live,
            unprobed: outcome.unprobed,
        }
    }
}

/// Run `prune [--json]`: open the per-user registry read-only, reap every
/// confirmed-stale entry, and report the tally — a human-readable summary line by
/// default, or a single JSON object with `--json`.
///
/// Uses [`registry::Registry::open_read_only`], not [`registry::Registry::open`]: prune
/// must never create the registry directory or touch its permissions just to reap it
/// (see the module docs above). A missing or empty registry prunes nothing and exits
/// `0`; the only failure is the registry directory itself being unreadable, an
/// [`exit::SETUP`] condition (a support/prerequisite failure).
pub fn run(json: bool) -> Result<(), RunnerError> {
    let registry = registry::Registry::open_read_only().map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not open the run registry: {err}"),
        )
    })?;
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

/// Print a concise, human-readable summary line: how many stale entries were reaped,
/// and — when any were kept back — how many live and how many unprobeable ones were
/// deliberately left alone.
fn print_summary(outcome: PruneOutcome) {
    if outcome.pruned == 0 && outcome.live == 0 && outcome.unprobed == 0 {
        println!("no stale entries to prune");
        return;
    }
    println!(
        "pruned {} stale, kept {} live, left {} unprobeable",
        outcome.pruned, outcome.live, outcome.unprobed
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
        });
        let json = serde_json::to_string(&report).expect("a prune report serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["pruned"], 2);
        assert_eq!(value["live"], 1);
        assert_eq!(value["unprobed"], 3);
    }
}
