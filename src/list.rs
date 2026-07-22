//! `list`: enumerate every run recorded in the per-user registry.
//!
//! The by-`run_id` commands (`inspect`/`cancel`/`kill`) all require an operator or
//! orchestrator to already know which run to target; `list` is the discovery
//! counterpart — it scans the registry ([`registry::Registry::entries`]) and prints
//! every entry it finds, live or stale, so a caller that lost (or never had) a
//! `run_id` can find one. It is read-only: it never connects to a runner's control
//! transport and never mutates the registry, so it carries none of the
//! reach-a-live-runner failure modes `inspect`/`cancel`/`kill` do (see
//! `src/control.rs`) — the only way it can fail is the registry directory itself
//! being unreadable, which is a [`exit::SETUP`] condition (a support/prerequisite
//! failure), not a [`exit::CONTROL`] one (which is reserved for "could not reach
//! *this specific target run*", a concept `list` has no single instance of).
//!
//! An empty registry is not an error — it is a normal, if unglamorous, discovery
//! result — so `list` prints an empty result and exits `0` either way. A single
//! corrupt/unreadable record never blinds the command to the healthy entries: that
//! degradation already lives in [`registry::Registry::entries`], so this module
//! does not need to (and does not) duplicate it.

use serde::Serialize;

use crate::exit::{self, RunnerError};
use crate::registry::{self, Health};

/// One `list` entry as printed — the client's own display/JSON shape, decoupled
/// from the on-disk [`registry::Record`] format so it can be rendered as a
/// human-readable row or serialized as JSON without leaking registry-internal
/// fields (the lock file name, the registry format version) that a caller of
/// `list` has no use for.
#[derive(Debug, Serialize)]
struct ListEntry {
    /// The run's identifier — the value a caller passes as `--run-id` to
    /// `inspect`/`cancel`/`kill`.
    run_id: String,
    /// `"live"` or `"stale"`, the same vocabulary [`registry::Health`] documents:
    /// a live runner still holds its liveness lock; a stale entry is a leftover
    /// record from a runner that died abruptly without cleaning up.
    health: &'static str,
    /// Run start time, RFC 3339 UTC with millisecond precision (the same
    /// formatter every other timestamp in this binary uses).
    started_at: String,
    /// The run's local control-transport endpoint, or `None` when the transport
    /// was never stood up (best-effort degradation — see
    /// [`registry::Record::endpoint`]) — never populated for a stale entry's
    /// original runner, but still whatever the last-published record said.
    endpoint: Option<String>,
}

/// Run `list [--json]`: open the per-user registry, scan every entry, and print
/// them either as a human-readable table (default) or as one JSON object per line
/// (`--json`) — deterministically ordered by `run_id` then `started_at` so the
/// output is stable across runs of the same registry state.
pub fn run(json: bool) -> Result<(), RunnerError> {
    let registry = registry::Registry::open().map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not open the run registry: {err}"),
        )
    })?;
    let entries = registry.entries().map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not read the run registry: {err}"),
        )
    })?;

    let mut rows: Vec<ListEntry> = entries
        .into_iter()
        .map(|entry| ListEntry {
            run_id: entry.record.run_id,
            health: health_str(entry.health),
            started_at: entry.record.started_at,
            endpoint: entry.record.endpoint,
        })
        .collect();
    rows.sort_by(|a, b| {
        a.run_id
            .cmp(&b.run_id)
            .then_with(|| a.started_at.cmp(&b.started_at))
    });

    if json {
        print_json(&rows)
    } else {
        print_table(&rows);
        Ok(())
    }
}

/// `health` rendered in the vocabulary `list` prints and serializes — never the
/// `Debug` form, so the output is a stable, documented contract independent of how
/// [`registry::Health`]'s derive happens to render.
fn health_str(health: Health) -> &'static str {
    match health {
        Health::Live => "live",
        Health::Stale => "stale",
    }
}

/// Print one JSON object per entry, one per line — the same "JSON Lines" shape
/// `--json` uses elsewhere in this binary's machine-readable output.
fn print_json(rows: &[ListEntry]) -> Result<(), RunnerError> {
    for row in rows {
        let line = serde_json::to_string(row).map_err(|err| {
            RunnerError::new(exit::SETUP, format!("could not render a list entry: {err}"))
        })?;
        println!("{line}");
    }
    Ok(())
}

/// Print a simple, aligned, human-readable table. An empty registry prints a
/// one-line notice rather than a bare header with no rows.
fn print_table(rows: &[ListEntry]) {
    if rows.is_empty() {
        println!("no runs registered");
        return;
    }
    println!("RUN_ID  HEALTH  STARTED_AT  ENDPOINT");
    for row in rows {
        println!(
            "{}  {}  {}  {}",
            row.run_id,
            row.health,
            row.started_at,
            row.endpoint.as_deref().unwrap_or("-")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_str_uses_the_documented_vocabulary() {
        assert_eq!(health_str(Health::Live), "live");
        assert_eq!(health_str(Health::Stale), "stale");
    }

    /// A `ListEntry` round-trips through JSON with the exact field names a
    /// consumer of `list --json` depends on.
    #[test]
    fn list_entry_serializes_the_documented_fields() {
        let entry = ListEntry {
            run_id: "run-1".to_string(),
            health: "live",
            started_at: "2026-07-22T00:00:00.000Z".to_string(),
            endpoint: Some("/tmp/pkc-x/c.sock".to_string()),
        };
        let json = serde_json::to_string(&entry).expect("a list entry serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["run_id"], "run-1");
        assert_eq!(value["health"], "live");
        assert_eq!(value["started_at"], "2026-07-22T00:00:00.000Z");
        assert_eq!(value["endpoint"], "/tmp/pkc-x/c.sock");
    }

    /// A `None` endpoint serializes as JSON `null`, not an absent field — a
    /// consumer can always index `["endpoint"]`.
    #[test]
    fn list_entry_serializes_a_missing_endpoint_as_null() {
        let entry = ListEntry {
            run_id: "run-2".to_string(),
            health: "stale",
            started_at: "2026-07-22T00:00:00.000Z".to_string(),
            endpoint: None,
        };
        let json = serde_json::to_string(&entry).expect("a list entry serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert!(value["endpoint"].is_null());
    }
}
