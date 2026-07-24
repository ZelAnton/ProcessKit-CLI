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

use std::path::PathBuf;

use serde::Serialize;

use crate::exit::{self, RunnerError};
use crate::registry::{self, Health};

/// One `list` entry as printed — the client's own display/JSON shape, decoupled
/// from the on-disk [`registry::Record`] format so it can be rendered as a
/// human-readable row or serialized as JSON without leaking registry-internal
/// fields (the lock file name, the registry format version) that a caller of
/// `list` has no use for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

/// Run `list [--json]`: open the per-user registry read-only, scan every entry, and
/// print them either as a human-readable table (default) or as one JSON object per
/// line (`--json`) — deterministically ordered by `run_id` then `started_at`, with
/// each entry's registry record path as a tertiary tie-breaker (two records can
/// legitimately share both a `run_id` and a millisecond-precision `started_at`; the
/// record path is unique per on-disk entry, so it makes the order fully
/// deterministic without leaking into the printed/serialized shape) so the output
/// is stable across runs of the same registry state.
///
/// Uses [`registry::Registry::open_read_only`], not [`registry::Registry::open`]:
/// `list` must never create the registry directory or touch its permissions just to
/// scan it (see the module docs above) — that mutating path exists only for `run`,
/// which is actually about to write a record.
pub fn run(json: bool) -> Result<(), RunnerError> {
    let registry = registry::Registry::open_read_only().map_err(|err| {
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

    let mut rows: Vec<(PathBuf, ListEntry)> = entries
        .into_iter()
        .map(|entry| {
            let path = entry.path;
            let list_entry = ListEntry {
                run_id: entry.record.run_id,
                health: health_str(entry.health),
                started_at: entry.record.started_at,
                endpoint: entry.record.endpoint,
            };
            (path, list_entry)
        })
        .collect();
    sort_rows(&mut rows);
    let rows: Vec<ListEntry> = rows.into_iter().map(|(_, entry)| entry).collect();

    if json {
        print_json(&rows)
    } else {
        print_table(&rows);
        Ok(())
    }
}

/// Order `rows` by `run_id`, then `started_at`, then the entry's registry record
/// path — the tertiary key exists purely to make the order fully deterministic when
/// two entries legitimately share both a `run_id` and a millisecond-precision
/// `started_at` (see [`run`]'s docs); it is never printed or serialized.
fn sort_rows(rows: &mut [(PathBuf, ListEntry)]) {
    rows.sort_by(|(a_path, a), (b_path, b)| {
        a.run_id
            .cmp(&b.run_id)
            .then_with(|| a.started_at.cmp(&b.started_at))
            .then_with(|| a_path.cmp(b_path))
    });
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

/// Print a column-aligned, human-readable table: every column is padded to
/// the width of its widest value (header included), so rows line up under
/// the header regardless of value length. An empty registry prints a
/// one-line notice rather than a bare header with no rows.
fn print_table(rows: &[ListEntry]) {
    for line in render_table_lines(rows) {
        println!("{line}");
    }
}

/// Build the lines `print_table` would print, without touching stdout —
/// split out so alignment can be asserted directly in tests.
fn render_table_lines(rows: &[ListEntry]) -> Vec<String> {
    if rows.is_empty() {
        return vec!["no runs registered".to_string()];
    }
    const HEADERS: [&str; 4] = ["RUN_ID", "HEALTH", "STARTED_AT", "ENDPOINT"];
    let cells: Vec<[String; 4]> = rows
        .iter()
        .map(|row| {
            [
                row.run_id.clone(),
                row.health.to_string(),
                row.started_at.clone(),
                row.endpoint.as_deref().unwrap_or("-").to_string(),
            ]
        })
        .collect();
    let mut widths = HEADERS.map(str::len);
    for row in &cells {
        for (width, cell) in widths.iter_mut().zip(row.iter()) {
            *width = (*width).max(cell.len());
        }
    }
    // The last column is left-aligned but not padded, so trailing whitespace
    // is never printed after the final value.
    let mut lines = Vec::with_capacity(cells.len() + 1);
    lines.push(format!(
        "{:w0$}  {:w1$}  {:w2$}  {}",
        HEADERS[0],
        HEADERS[1],
        HEADERS[2],
        HEADERS[3],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
    ));
    for row in &cells {
        lines.push(format!(
            "{:w0$}  {:w1$}  {:w2$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
        ));
    }
    lines
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

    /// Two entries sharing both `run_id` and `started_at` (a millisecond collision
    /// is possible in principle) must still sort deterministically — on their
    /// registry record path, the tertiary key — rather than falling back to
    /// whatever order the registry scan happened to hand them in.
    #[test]
    fn sort_rows_breaks_run_id_and_started_at_ties_on_the_record_path() {
        let entry = |suffix: &str| ListEntry {
            run_id: "same-run-id".to_string(),
            health: "live",
            started_at: "2026-07-22T00:00:00.000Z".to_string(),
            endpoint: Some(format!("/tmp/pkc-{suffix}.sock")),
        };
        let path = |name: &str| PathBuf::from(name);

        // Deliberately fed in the "wrong" (path-descending) order; a correct sort
        // must still land them path-ascending.
        let mut rows = vec![
            (path("c-run.json"), entry("c")),
            (path("a-run.json"), entry("a")),
            (path("b-run.json"), entry("b")),
        ];
        sort_rows(&mut rows);

        let ordered_paths: Vec<&PathBuf> = rows.iter().map(|(path, _)| path).collect();
        assert_eq!(
            ordered_paths,
            vec![
                &path("a-run.json"),
                &path("b-run.json"),
                &path("c-run.json"),
            ],
            "identical run_id/started_at must tie-break on the record path"
        );

        // Sorting the exact same input again yields the exact same order — the sort
        // is deterministic, not merely "some" total order.
        let mut rows_again = vec![
            (path("c-run.json"), entry("c")),
            (path("a-run.json"), entry("a")),
            (path("b-run.json"), entry("b")),
        ];
        sort_rows(&mut rows_again);
        assert_eq!(rows, rows_again, "sorting is repeatable across runs");
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

    /// An empty registry renders as the one-line notice, not a bare header.
    #[test]
    fn render_table_lines_reports_an_empty_registry_with_a_notice() {
        assert_eq!(render_table_lines(&[]), vec!["no runs registered"]);
    }

    /// Every column is padded to its widest value (header included), so a
    /// short `run_id` in one row and a long one in another still line up
    /// under the header — matching the docstring's "column-aligned" claim.
    #[test]
    fn render_table_lines_pads_every_column_to_its_widest_value() {
        let rows = vec![
            ListEntry {
                run_id: "r1".to_string(),
                health: "live",
                started_at: "2026-07-22T00:00:00.000Z".to_string(),
                endpoint: Some("/tmp/pkc-a.sock".to_string()),
            },
            ListEntry {
                run_id: "much-longer-run-id".to_string(),
                health: "stale",
                started_at: "2026-07-22T00:00:00.000Z".to_string(),
                endpoint: None,
            },
        ];
        let lines = render_table_lines(&rows);
        assert_eq!(
            lines,
            vec![
                "RUN_ID              HEALTH  STARTED_AT                ENDPOINT",
                "r1                  live    2026-07-22T00:00:00.000Z  /tmp/pkc-a.sock",
                "much-longer-run-id  stale   2026-07-22T00:00:00.000Z  -",
            ]
        );
        // The actual alignment property: the HEALTH value starts at the same
        // column in every row as the "HEALTH" header does, regardless of how
        // long that row's `run_id` is.
        let health_col = lines[0].find("HEALTH").unwrap();
        assert_eq!(lines[1][health_col..].find("live"), Some(0));
        assert_eq!(lines[2][health_col..].find("stale"), Some(0));
    }
}
