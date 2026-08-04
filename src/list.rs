//! `list`: enumerate every run recorded in the per-user registry.
//!
//! The by-`run_id` commands (`inspect`/`cancel`/`kill`/`attest`) all require an
//! operator or orchestrator to already know which run to target; `list` is the
//! discovery counterpart — it scans the registry ([`registry::Registry::entries`])
//! and prints every entry it finds, whatever its health, so a caller that lost (or
//! never had) a `run_id` can find one. It is read-only: it never connects to a
//! runner's control
//! transport and never mutates the registry, so it carries none of the
//! reach-a-live-runner failure modes `inspect`/`cancel`/`kill`/`attest` do (see
//! `src/control/mod.rs`) — the only way it can fail is the registry directory itself
//! being unreadable, which is a [`exit::SETUP`] condition (a support/prerequisite
//! failure), not a [`exit::CONTROL`] one (which is reserved for "could not reach
//! *this specific target run*", a concept `list` has no single instance of).
//!
//! An empty registry is not an error — it is a normal, if unglamorous, discovery
//! result — so `list` prints an empty result and exits `0` either way. A single
//! corrupt/unreadable record never blinds the command to the healthy entries: that
//! degradation already lives in [`registry::Registry::entries`], so this module
//! does not need to (and does not) duplicate it.
//!
//! **Health vocabulary (T-206).** [`registry::Health`] has three values, not two:
//! [`registry::Health::Live`], [`registry::Health::Stale`] (**confirmed** dead — the
//! probe succeeded and found no holder), and [`registry::Health::Unprobed`] (the
//! probe itself could not run — e.g. permission denied, or the lock file replaced by
//! something the probe refuses to open — so liveness is genuinely unknown). Printing
//! an unprobed entry as `"stale"` would assert a confirmed death the probe never
//! established; [`health_str`] renders it as its own `"unprobed"` value instead, the
//! same vocabulary `prune --json`'s tallies and `wait`'s `RunStatus::Unprobed`
//! already use for the identical case (`docs/registry.md`). This is additive to
//! `list --json`'s existing `"live"`/`"stale"` contract.
//!
//! **Which run is which (T-215).** Discovery is only half-done if every row looks
//! alike: an operator with three live runs used to see three ids, three timestamps,
//! and three endpoints, with nothing saying *what* any of them was running — so
//! choosing the one to `inspect`/`cancel`/`kill` meant guessing. Each entry now also
//! carries the two **redaction-safe** command fields the registry record publishes
//! (see [`registry::Record::argv_sha256`], [`registry::Record::hint`]): a one-way
//! argv fingerprint and a categorical worker-shape hint, the same pair the JSONL
//! `run_started` event carries, derived from the same code. Neither can disclose a
//! command line — that is exactly why they can be shown here — yet together they
//! answer the operator's actual question: which of these rows are the same command,
//! and which is the build worker. Both are additive: `--json` gains two fields
//! (`null` on a record that carries neither, e.g. one written before they existed),
//! and the table gains two columns, the fingerprint abbreviated (see
//! [`abbreviated_argv_sha256`]) with the full digest reserved for `--json`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::cli::ListHealth;
use crate::exit::{self, RunnerError};
use crate::labels::OperatorLabel;
use crate::registry::{self, Health};

/// One `list` entry as printed — the client's own display/JSON shape, decoupled
/// from the on-disk [`registry::Record`] format so it can be rendered as a
/// human-readable row or serialized as JSON without leaking registry-internal
/// fields (the lock file name, the registry format version) that a caller of
/// `list` has no use for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ListEntry {
    /// The run's identifier — the value a caller passes as `--run-id` to
    /// `inspect`/`cancel`/`kill`/`attest`.
    run_id: String,
    /// `"live"`, `"stale"`, or `"unprobed"` — the same three-value vocabulary
    /// [`registry::Health`] documents, and the one `prune --json`'s tallies and
    /// `wait`'s `RunStatus` already use for the same distinction (see
    /// `docs/registry.md`): a live runner still holds its liveness lock; a stale
    /// entry is a **confirmed** leftover record from a runner that died abruptly
    /// without cleaning up; an unprobed entry is one whose liveness lock could not
    /// even be opened (e.g. permission denied, or an unexpected non-regular file in
    /// its place) — its fate is genuinely unknown, never a positive "the runner is
    /// dead" claim. New, additive value as of the value's introduction — a consumer
    /// that already handles `"live"`/`"stale"` and treats any other string as
    /// "not live" needs no change; one that matched exhaustively on exactly those two
    /// strings must be updated to accept `"unprobed"` too.
    health: &'static str,
    /// Run start time, RFC 3339 UTC with millisecond precision (the same
    /// formatter every other timestamp in this binary uses).
    started_at: String,
    /// The run's worker-shape category (`msbuild_node_reuse`, …), or `None` when
    /// the run's command matched no known shape — the common case. Straight from
    /// [`registry::Record::hint`], which is the same classifier catalog the JSONL
    /// `run_started` event uses (`docs/schema.md`, "Hint classifier"). A fixed
    /// category label, never command-line content.
    hint: Option<String>,
    /// The run's one-way argv fingerprint — lowercase-hex SHA-256, in **full**
    /// here, so a caller can compare it against the `argv_sha256` of the same
    /// run's `run_started` event byte for byte (the human-readable table
    /// abbreviates it instead, see [`abbreviated_argv_sha256`]). `None` for a
    /// record written before the field existed, or one whose value did not survive
    /// the registry's read-side shape guard.
    ///
    /// This plus `hint` are what make `list` usable as the discovery step it is
    /// meant to be (T-215): several live runs are no longer an undifferentiated
    /// list of ids and timestamps — the fingerprint says which of them are running
    /// the *same* command, without disclosing that command to anyone.
    argv_sha256: Option<String>,
    /// Operator labels in full for machine-readable discovery.
    labels: BTreeMap<String, String>,
    /// Absolute path to the JSONL lifecycle stream, when the record publishes it.
    jsonl: Option<String>,
    /// Absolute capture directory, when output capture is enabled.
    capture_dir: Option<String>,
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
pub fn run(
    json: bool,
    labels: &[OperatorLabel],
    health: Option<ListHealth>,
) -> Result<(), RunnerError> {
    let registry = registry::open_read_only_for_setup()?;
    let entries = registry.entries().map_err(registry::setup_read_error)?;

    let mut rows: Vec<(PathBuf, ListEntry)> = entries
        .into_iter()
        .filter(|entry| {
            crate::labels::matches(&entry.record.labels, labels)
                && health.is_none_or(|expected| health_matches(entry.health, expected))
        })
        .map(|entry| {
            let path = entry.path;
            let list_entry = ListEntry {
                run_id: entry.record.run_id,
                health: health_str(entry.health),
                started_at: entry.record.started_at,
                hint: entry.record.hint,
                argv_sha256: entry.record.argv_sha256,
                labels: entry.record.labels,
                jsonl: entry.record.jsonl,
                capture_dir: entry.record.capture_dir,
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

fn health_matches(actual: Health, expected: ListHealth) -> bool {
    matches!(
        (actual, expected),
        (Health::Live, ListHealth::Live)
            | (Health::Stale, ListHealth::Stale)
            | (Health::Unprobed, ListHealth::Unprobed)
    )
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
/// [`registry::Health`]'s derive happens to render. Deliberately no wildcard arm: a
/// future [`registry::Health`] variant fails this module's build until `list`'s
/// vocabulary is extended for it too, rather than silently omitting it from the
/// operator-facing output.
fn health_str(health: Health) -> &'static str {
    match health {
        Health::Live => "live",
        Health::Stale => "stale",
        Health::Unprobed => "unprobed",
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

/// How many leading hex characters of an `argv_sha256` the human-readable table
/// shows. The full digest is 64 characters — six times the width of every other
/// column put together — and printing it would push `ENDPOINT` off the far right of
/// any ordinary terminal for a field an operator reads *comparatively* ("these two
/// rows are the same command, that one is not"), not character by character. Twelve
/// hex characters are 48 bits: two distinct commands among a handful of live runs
/// colliding on them is not a case worth widening every row for, and the exact,
/// full-length value is one `--json` away — which is where a caller that means to
/// *match* a fingerprint against the run's `run_started` event should read it from
/// anyway.
const ARGV_SHA256_TABLE_CHARS: usize = 12;

/// The marker appended to an abbreviated fingerprint in the table, so a truncated
/// value can never be mistaken for a whole digest (and copied somewhere as one).
const ABBREVIATION_MARKER: &str = "...";

/// The cell shown for a field the record does not carry — a record written before
/// the field existed, one whose value failed the registry's read-side shape guard,
/// or, for `hint`, the ordinary case of a command matching no known worker shape.
/// The same `-` `ENDPOINT` has always used for its own absent value.
const ABSENT_CELL: &str = "-";

/// The table's rendering of an `argv_sha256`: its first [`ARGV_SHA256_TABLE_CHARS`]
/// hex characters plus [`ABBREVIATION_MARKER`], or [`ABSENT_CELL`] when the record
/// carries none.
///
/// Sliced on **characters, not bytes**, so a value that is somehow not the ASCII hex
/// the registry's guard admits (impossible through [`registry::Registry::entries`],
/// which sanitizes it, but this function is also reachable in tests and by any future
/// caller) truncates on a character boundary instead of panicking. A value shorter
/// than the abbreviation is printed whole, unmarked — there is nothing hidden to
/// warn about.
fn abbreviated_argv_sha256(argv_sha256: Option<&str>) -> String {
    let Some(value) = argv_sha256 else {
        return ABSENT_CELL.to_string();
    };
    let mut chars = value.chars();
    let abbreviated: String = chars.by_ref().take(ARGV_SHA256_TABLE_CHARS).collect();
    if chars.next().is_none() {
        // Nothing was left over, so nothing was hidden — print it unmarked.
        abbreviated
    } else {
        format!("{abbreviated}{ABBREVIATION_MARKER}")
    }
}

fn rendered_labels(labels: &BTreeMap<String, String>) -> String {
    if labels.is_empty() {
        return ABSENT_CELL.to_string();
    }
    let rendered = labels
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                crate::text::terminal_safe(key),
                crate::text::terminal_safe(value)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    crate::text::terminal_safe_bounded(&rendered)
}

/// Build the lines `print_table` would print, without touching stdout —
/// split out so alignment can be asserted directly in tests.
fn render_table_lines(rows: &[ListEntry]) -> Vec<String> {
    if rows.is_empty() {
        return vec!["no runs registered".to_string()];
    }
    const HEADERS: [&str; 9] = [
        "RUN_ID",
        "HEALTH",
        "STARTED_AT",
        "HINT",
        "ARGV_SHA256",
        "LABELS",
        "JSONL",
        "CAPTURE_DIR",
        "ENDPOINT",
    ];
    let cells: Vec<[String; 9]> = rows
        .iter()
        .map(|row| {
            [
                crate::text::terminal_safe_bounded(&row.run_id),
                row.health.to_string(),
                row.started_at.clone(),
                row.hint.as_deref().unwrap_or(ABSENT_CELL).to_string(),
                abbreviated_argv_sha256(row.argv_sha256.as_deref()),
                rendered_labels(&row.labels),
                crate::text::terminal_safe_bounded(row.jsonl.as_deref().unwrap_or(ABSENT_CELL)),
                crate::text::terminal_safe_bounded(
                    row.capture_dir.as_deref().unwrap_or(ABSENT_CELL),
                ),
                crate::text::terminal_safe_bounded(row.endpoint.as_deref().unwrap_or(ABSENT_CELL)),
            ]
        })
        .collect();
    crate::text::aligned_table(HEADERS, &cells, "", "  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_str_uses_the_documented_vocabulary() {
        assert_eq!(health_str(Health::Live), "live");
        assert_eq!(health_str(Health::Stale), "stale");
        assert_eq!(health_str(Health::Unprobed), "unprobed");
    }

    #[test]
    fn health_filter_maps_exactly_to_the_registry_vocabulary() {
        for (actual, expected) in [
            (Health::Live, ListHealth::Live),
            (Health::Stale, ListHealth::Stale),
            (Health::Unprobed, ListHealth::Unprobed),
        ] {
            assert!(health_matches(actual, expected));
        }
        assert!(!health_matches(Health::Live, ListHealth::Stale));
        assert!(!health_matches(Health::Unprobed, ListHealth::Live));
    }

    /// The full-length fingerprint of a fixture entry — 64 lowercase hex
    /// characters, the shape `registry::Record::argv_sha256` carries.
    const FINGERPRINT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A `ListEntry` round-trips through JSON with the exact field names a
    /// consumer of `list --json` depends on — including the two redaction-safe
    /// command fields (T-215), whose whole point is that a consumer can read them
    /// machine-side at full precision.
    #[test]
    fn list_entry_serializes_the_documented_fields() {
        let entry = ListEntry {
            run_id: "run-1".to_string(),
            health: "live",
            started_at: "2026-07-22T00:00:00.000Z".to_string(),
            hint: Some("msbuild_node_reuse".to_string()),
            argv_sha256: Some(FINGERPRINT.to_string()),
            labels: [("batch".to_string(), "42".to_string())]
                .into_iter()
                .collect(),
            jsonl: Some("/runs/run-1.jsonl".to_string()),
            capture_dir: Some("/runs/run-1".to_string()),
            endpoint: Some("/tmp/pkc-x/c.sock".to_string()),
        };
        let json = serde_json::to_string(&entry).expect("a list entry serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["run_id"], "run-1");
        assert_eq!(value["health"], "live");
        assert_eq!(value["started_at"], "2026-07-22T00:00:00.000Z");
        assert_eq!(value["hint"], "msbuild_node_reuse");
        assert_eq!(
            value["argv_sha256"], FINGERPRINT,
            "--json carries the whole digest, never the table's abbreviation: {value}"
        );
        assert_eq!(value["labels"]["batch"], "42");
        assert_eq!(value["jsonl"], "/runs/run-1.jsonl");
        assert_eq!(value["capture_dir"], "/runs/run-1");
        assert_eq!(value["endpoint"], "/tmp/pkc-x/c.sock");
    }

    /// A record that carries neither command field — one written before they
    /// existed, or one whose values failed the registry's read-side shape guard —
    /// serializes them as JSON `null`, present and indexable, exactly as an absent
    /// `endpoint` already does. A consumer never has to distinguish "absent field"
    /// from "null field".
    #[test]
    fn list_entry_serializes_missing_command_fields_as_null() {
        let entry = ListEntry {
            run_id: "run-legacy".to_string(),
            health: "stale",
            started_at: "2026-07-22T00:00:00.000Z".to_string(),
            hint: None,
            argv_sha256: None,
            labels: BTreeMap::new(),
            jsonl: None,
            capture_dir: None,
            endpoint: None,
        };
        let json = serde_json::to_string(&entry).expect("a list entry serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert!(value["hint"].is_null(), "an absent hint is null: {value}");
        assert!(
            value["argv_sha256"].is_null(),
            "an absent fingerprint is null: {value}"
        );
        assert_eq!(value["labels"], serde_json::json!({}));
    }

    /// The table abbreviates a fingerprint to a fixed prefix plus an explicit
    /// marker, so a truncated value is self-evidently truncated (never copied
    /// somewhere as if it were a whole digest), and renders an absent one as `-`.
    #[test]
    fn abbreviated_argv_sha256_marks_what_it_truncates() {
        assert_eq!(
            abbreviated_argv_sha256(Some(FINGERPRINT)),
            "0123456789ab...",
            "a full digest shows its first {ARGV_SHA256_TABLE_CHARS} characters, marked as abbreviated"
        );
        assert_eq!(
            abbreviated_argv_sha256(None),
            "-",
            "an absent fingerprint renders like an absent endpoint"
        );
        // Nothing was hidden, so nothing is marked: a value no longer than the
        // abbreviation prints whole.
        assert_eq!(abbreviated_argv_sha256(Some("abc")), "abc");
        assert_eq!(
            abbreviated_argv_sha256(Some("0123456789ab")),
            "0123456789ab",
            "a value exactly the abbreviation's length is not marked as truncated"
        );
        // Multi-byte input cannot make the character-wise slice panic (the registry
        // guard never admits one, but this function must not depend on that).
        assert_eq!(
            abbreviated_argv_sha256(Some("ααααααααααααα")),
            "αααααααααααα..."
        );
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
            hint: None,
            argv_sha256: Some(FINGERPRINT.to_string()),
            labels: BTreeMap::new(),
            jsonl: None,
            capture_dir: None,
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
            hint: None,
            argv_sha256: Some(FINGERPRINT.to_string()),
            labels: BTreeMap::new(),
            jsonl: None,
            capture_dir: None,
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
                hint: Some("msbuild_node_reuse".to_string()),
                argv_sha256: Some(FINGERPRINT.to_string()),
                labels: BTreeMap::new(),
                jsonl: None,
                capture_dir: None,
                endpoint: Some("/tmp/pkc-a.sock".to_string()),
            },
            ListEntry {
                run_id: "much-longer-run-id".to_string(),
                health: "stale",
                started_at: "2026-07-22T00:00:00.000Z".to_string(),
                hint: None,
                argv_sha256: None,
                labels: BTreeMap::new(),
                jsonl: None,
                capture_dir: None,
                endpoint: None,
            },
        ];
        let lines = render_table_lines(&rows);
        assert_eq!(
            lines,
            vec![
                "RUN_ID              HEALTH  STARTED_AT                HINT                ARGV_SHA256      LABELS  JSONL  CAPTURE_DIR  ENDPOINT",
                "r1                  live    2026-07-22T00:00:00.000Z  msbuild_node_reuse  0123456789ab...  -       -      -            /tmp/pkc-a.sock",
                "much-longer-run-id  stale   2026-07-22T00:00:00.000Z  -                   -                -       -      -            -",
            ]
        );
        // The actual alignment property: a value starts at the same column in every
        // row as its header does, regardless of how long the values to its left are
        // — asserted for the last padded column (ARGV_SHA256) as well as the first,
        // so a column added in between cannot silently break the ones after it.
        let health_col = lines[0].find("HEALTH").unwrap();
        assert_eq!(lines[1][health_col..].find("live"), Some(0));
        assert_eq!(lines[2][health_col..].find("stale"), Some(0));
        let fingerprint_col = lines[0].find("ARGV_SHA256").unwrap();
        assert_eq!(lines[1][fingerprint_col..].find("0123456789ab..."), Some(0));
        assert_eq!(lines[2][fingerprint_col..].find('-'), Some(0));
        // No line ends in whitespace: the last column is deliberately unpadded.
        for line in &lines {
            assert_eq!(line.trim_end(), line, "no trailing whitespace: {line:?}");
        }
    }

    #[test]
    fn human_table_sanitizes_registry_controls_without_changing_json_data() {
        let row = ListEntry {
            run_id: "forged\nROW\u{1b}[31m".to_string(),
            health: "live",
            started_at: "2026-07-22T00:00:00.000Z".to_string(),
            hint: None,
            argv_sha256: None,
            labels: [("danger".to_string(), "bidi\u{202e}value".to_string())]
                .into_iter()
                .collect(),
            jsonl: None,
            capture_dir: None,
            endpoint: Some("pipe\tname\u{7}".to_string()),
        };

        let lines = render_table_lines(std::slice::from_ref(&row));
        assert_eq!(lines.len(), 2, "a forged newline cannot add a table row");
        assert!(
            lines
                .iter()
                .all(|line| line.chars().all(|character| !character.is_control())),
            "human-readable cells contain no terminal controls: {lines:?}"
        );
        assert!(lines[1].contains("forged ROW [31m"));
        assert!(!lines[1].contains('\u{202e}'));
        assert!(lines[1].ends_with("pipe name"));

        let json = serde_json::to_string(&row).expect("the raw JSON row serializes safely");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["run_id"], "forged\nROW\u{1b}[31m");
        assert_eq!(value["endpoint"], "pipe\tname\u{7}");
        assert_eq!(value["labels"]["danger"], "bidi\u{202e}value");
    }

    #[test]
    fn human_table_bounds_untrusted_identity_and_endpoint_cells() {
        let oversized_run_id = "r".repeat(crate::text::TERMINAL_FIELD_MAX_CHARS + 20);
        let oversized_endpoint = "e".repeat(crate::text::TERMINAL_FIELD_MAX_CHARS + 20);
        let mut labels = BTreeMap::new();
        labels.insert(
            "batch".to_string(),
            "l".repeat(crate::text::TERMINAL_FIELD_MAX_CHARS + 20),
        );
        let row = ListEntry {
            run_id: oversized_run_id.clone(),
            health: "live",
            started_at: "2026-07-22T00:00:00.000Z".to_string(),
            hint: None,
            argv_sha256: None,
            labels: labels.clone(),
            jsonl: None,
            capture_dir: None,
            endpoint: Some(oversized_endpoint.clone()),
        };

        let lines = render_table_lines(std::slice::from_ref(&row));
        assert!(lines[1].contains(&format!(
            "{}...",
            "r".repeat(crate::text::TERMINAL_FIELD_MAX_CHARS)
        )));
        assert!(lines[1].ends_with(&format!(
            "{}...",
            "e".repeat(crate::text::TERMINAL_FIELD_MAX_CHARS)
        )));
        assert!(lines[1].contains(&format!(
            "batch={}...",
            "l".repeat(crate::text::TERMINAL_FIELD_MAX_CHARS - "batch=".len())
        )));

        let json = serde_json::to_value(&row).expect("the raw JSON row serializes");
        assert_eq!(json["run_id"], oversized_run_id);
        assert_eq!(json["endpoint"], oversized_endpoint);
        assert_eq!(json["labels"]["batch"], labels["batch"]);
    }

    /// The property T-215 exists for, at the surface an operator actually reads:
    /// two *live* runs that differ only in their command are distinguishable in the
    /// human-readable table — the one thing the pre-T-215 table could not do, since
    /// `run_id`s can be opaque, health is identical for both, `started_at` can
    /// collide at millisecond precision, and an endpoint is an address, not a
    /// description. A third row running the *same* command as the first shows the
    /// identical fingerprint, so the table answers "which of these are the same
    /// command?" too, and none of it discloses a command line.
    #[test]
    fn render_table_lines_tells_two_live_runs_apart_by_their_command() {
        let build_fingerprint = "aaaaaaaaaaaabbbbbbbbbbbbccccccccccccddddddddddddeeeeeeeeeeeeffff";
        let other_fingerprint = "111111111111222222222222333333333333444444444444555555555555ffff";
        let entry = |run_id: &str, hint: Option<&str>, fingerprint: &str| ListEntry {
            run_id: run_id.to_string(),
            health: "live",
            // Deliberately identical across all three rows: neither health nor the
            // start time can be what distinguishes them here.
            started_at: "2026-07-22T00:00:00.000Z".to_string(),
            hint: hint.map(str::to_string),
            argv_sha256: Some(fingerprint.to_string()),
            labels: BTreeMap::new(),
            jsonl: None,
            capture_dir: None,
            endpoint: None,
        };
        let rows = vec![
            entry("run-a", Some("msbuild_node_reuse"), build_fingerprint),
            entry("run-b", None, other_fingerprint),
            entry("run-c", Some("msbuild_node_reuse"), build_fingerprint),
        ];
        let lines = render_table_lines(&rows);

        let cell = |line: &str, header: &str| {
            let column = lines[0].find(header).expect("the header names the column");
            line[column..]
                .split_whitespace()
                .next()
                .expect("the column has a value")
                .to_string()
        };
        let (a, b, c) = (&lines[1], &lines[2], &lines[3]);
        assert_ne!(
            cell(a, "ARGV_SHA256"),
            cell(b, "ARGV_SHA256"),
            "two different commands must not print the same fingerprint"
        );
        assert_eq!(
            cell(a, "ARGV_SHA256"),
            cell(c, "ARGV_SHA256"),
            "the same command must print the same fingerprint"
        );
        assert_eq!(cell(a, "HINT"), "msbuild_node_reuse");
        assert_eq!(
            cell(b, "HINT"),
            "-",
            "an unclassified run shows the absent-value cell, not an invented label"
        );
        // Whatever the rows show, it is never the command line itself: only a
        // one-way digest and a fixed category label ever reach this table.
        for line in &lines {
            assert!(
                !line.contains("MSBuild.dll") && !line.contains("/nodeReuse"),
                "the table must never carry argv content: {line}"
            );
        }
    }
}
